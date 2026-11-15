//! 完整备份与本机快照的内容：
//!
//! ```text
//! termlet-archive/1\n
//! {说明（Manifest），一行 JSON}\n
//! gzip 压缩的数据库（VACUUM INTO 得到的一致副本）
//! ```
//!
//! 说明放在最前面：列出快照时只读这一行，不解压数据库。完整备份再把整个内容用密码加密（见 [`super::envelope`]）；
//! 本机快照不加密，其中的秘密仍是本机保险库加密的密文。主机指纹（`known_host` 表）就在数据库中。

use std::fs::File;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{BackupError, SnapshotReason, error};

const MAGIC: &[u8] = b"termlet-archive/1\n";
const FORMAT: u32 = 1;

/// 说明的大小上限。
pub(super) const MAX_MANIFEST_SIZE: u64 = 1 << 20;
/// 解压出的数据库的大小上限（直接写进临时文件，不读入内存）。
pub(super) const MAX_DB_SIZE: u64 = 2 << 30;

/// 审计日志的表（命令行工具的审计，R10）：完整备份可选是否包含；不包含时恢复后保留本机的。
pub(super) const AUDIT_TABLE: &str = "audit_log";

/// 说明的类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ArchiveKind {
    Backup,
    Snapshot,
}

/// 备份、快照中的说明。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Manifest {
    pub format: u32,
    pub kind: ArchiveKind,
    /// Unix 毫秒
    pub created_at: i64,
    #[serde(default)]
    pub app: String,
    /// 数据库的结构版本，高于本程序支持的版本时不能恢复
    pub schema: i64,
    /// 快照的原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<SnapshotReason>,
    /// 含审计日志；不含时恢复后保留本机的审计日志
    #[serde(default)]
    pub audit: bool,
    #[serde(default)]
    pub counts: Counts,
    /// 数据库内容的 SHA-256（自动快照去重用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// 完整备份中秘密与命令历史转加密用的随机密钥，只出现在加密的备份文件中
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_key: Option<DataKey>,
}

impl Manifest {
    pub fn new(kind: ArchiveKind, app: &str) -> Self {
        Self {
            format: FORMAT,
            kind,
            created_at: crate::time::now_ms(),
            app: app.to_string(),
            schema: 0,
            reason: None,
            audit: false,
            counts: Counts::default(),
            digest: None,
            data_key: None,
        }
    }
}

/// 转加密用的数据密钥：`Debug` 不显示内容，丢弃时清零。
#[derive(Clone)]
pub(super) struct DataKey(pub Zeroizing<[u8; 32]>);

impl DataKey {
    pub fn generate() -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut key[..]).expect("系统随机数不可用");
        Self(key)
    }
}

impl std::fmt::Debug for DataKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataKey(***)")
    }
}

impl Serialize for DataKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let hex = Zeroizing::new(data_encoding::HEXLOWER.encode(&self.0[..]));
        serializer.serialize_str(&hex)
    }
}

impl<'de> Deserialize<'de> for DataKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let hex = Zeroizing::new(String::deserialize(deserializer)?);
        let bytes = Zeroizing::new(
            data_encoding::HEXLOWER
                .decode(hex.as_bytes())
                .map_err(serde::de::Error::custom)?,
        );
        let key: [u8; 32] = bytes[..]
            .try_into()
            .map_err(|_| serde::de::Error::custom("数据密钥的长度不对"))?;
        Ok(Self(Zeroizing::new(key)))
    }
}

/// 备份、快照中各类数据的数量（恢复前给用户确认；当前数据见 [`super::Backups::current_counts`]）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Counts {
    pub hosts: usize,
    pub groups: usize,
    /// 凭据档案
    pub profiles: usize,
    /// 密钥库中的密钥
    pub keys: usize,
    /// 快捷命令
    pub snippets: usize,
    /// 端口转发规则
    pub forwards: usize,
    pub workspaces: usize,
    /// 自定义的终端配色
    pub schemes: usize,
    /// 命令历史的条数
    pub history: usize,
    /// 主机指纹
    pub known_hosts: usize,
    /// 审计日志的条数
    pub audit: usize,
}

/// 统计数据库中各类数据的数量（不含删除墓碑）。没有的表按 0（快照可能来自旧的结构版本）。
pub(super) fn count_rows(conn: &Connection) -> rusqlite::Result<Counts> {
    let count = |table: &str, items: bool| -> rusqlite::Result<usize> {
        if !table_exists(conn, table)? {
            return Ok(0);
        }
        let filter = if items {
            " WHERE deleted_at IS NULL"
        } else {
            ""
        };
        // 表名来自下面的固定列表
        let n: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}{filter}"), [], |r| {
            r.get(0)
        })?;
        Ok(n.max(0) as usize)
    };
    Ok(Counts {
        hosts: count("host", true)?,
        groups: count("host_group", true)?,
        profiles: count("profile", true)?,
        keys: count("ssh_key", true)?,
        snippets: count("snippet", true)?,
        forwards: count("forward", true)?,
        workspaces: count("workspace", true)?,
        schemes: count("color_scheme", true)?,
        history: count("command_history", false)?,
        known_hosts: count("known_host", false)?,
        audit: count(AUDIT_TABLE, false)?,
    })
}

pub(super) fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// 数据库副本的数量与结构版本。副本改为不用 WAL（只有一个文件，写进快照的就是完整的数据库）。
pub(super) fn inspect_copy(path: &Path) -> Result<(Counts, i64), BackupError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let _: String = conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0))?;
    let counts = count_rows(&conn)?;
    let schema: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok((counts, schema))
}

/// 文件内容的 SHA-256（小写十六进制）。
pub(super) fn digest(path: &Path) -> Result<String, BackupError> {
    let mut file = File::open(path).map_err(|e| error::read_error(path, &e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| error::read_error(path, &e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(data_encoding::HEXLOWER.encode(&hasher.finalize()[..]))
}

/// 写出：标记、说明、压缩后的数据库文件 `db`。
pub(super) fn write(out: impl Write, manifest: &Manifest, db: &Path) -> io::Result<()> {
    let mut out = out;
    out.write_all(MAGIC)?;
    serde_json::to_writer(&mut out, manifest)?;
    out.write_all(b"\n")?;
    let mut encoder = GzEncoder::new(out, Compression::default());
    io::copy(&mut File::open(db)?, &mut encoder)?;
    encoder.finish()?.flush()
}

/// 读出说明（读到说明之后为止，之后可以接着解压数据库）。
pub(super) fn read_manifest(reader: &mut impl BufRead) -> Result<Manifest, BackupError> {
    let mut magic = Vec::new();
    reader
        .by_ref()
        .take(MAGIC.len() as u64)
        .read_to_end(&mut magic)
        .map_err(|_| BackupError::Invalid)?;
    if magic != MAGIC {
        return Err(BackupError::Invalid);
    }
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(MAX_MANIFEST_SIZE + 1)
        .read_until(b'\n', &mut line)
        .map_err(|_| BackupError::Invalid)?;
    if line.pop() != Some(b'\n') {
        // 没有换行：过大或被截断
        return Err(BackupError::Invalid);
    }
    let manifest: Manifest = serde_json::from_slice(&line).map_err(|_| BackupError::Invalid)?;
    if manifest.format != FORMAT {
        return Err(BackupError::Invalid);
    }
    Ok(manifest)
}

/// 把说明之后压缩的数据库解压到 `dest`（边解压边写入）。超过 `limit` 字节或解压出错时返回
/// [`BackupError::Invalid`]（文件损坏或不是本程序写出的），写入出错时返回写入错误。
pub(super) fn extract(reader: impl Read, dest: &Path, limit: u64) -> Result<(), BackupError> {
    let mut decoder = GzDecoder::new(reader);
    let mut out = File::create(dest).map_err(|e| error::write_error(dest, &e))?;
    let mut buf = vec![0u8; 256 << 10];
    let mut total = 0u64;
    loop {
        let n = decoder.read(&mut buf).map_err(|_| BackupError::Invalid)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err(BackupError::Invalid);
        }
        out.write_all(&buf[..n])
            .map_err(|e| error::write_error(dest, &e))?;
    }
    out.sync_all().map_err(|e| error::write_error(dest, &e))
}

/// 备份目录中的临时数据库文件：丢弃时连同 SQLite 的附属文件一起删除。
pub(super) struct TempFile {
    path: PathBuf,
}

/// 临时文件名的前缀（启动时清理上次异常退出留下的，见 [`super::Backups::clean_temp`]）。
pub(super) const TEMP_PREFIX: &str = ".tmp-";

impl TempFile {
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(format!("{TEMP_PREFIX}{}.db", crate::id::random_token(6))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let mut name = self.path.clone().into_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(manifest: &Manifest, db: &[u8]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("db");
        std::fs::write(&file, db).unwrap();
        let mut out = Vec::new();
        write(&mut out, manifest, &file).unwrap();
        out
    }

    // 说明读入内存、有大小上限；数据库直接解压到文件，同样有上限
    #[test]
    fn manifest_and_database_have_limits() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out.db");
        let manifest = Manifest::new(ArchiveKind::Backup, "1.2.3");
        let data = archive(&manifest, b"db");
        let mut reader = &data[..];
        let read = read_manifest(&mut reader).unwrap();
        assert_eq!(
            (read.kind, read.app.as_str()),
            (ArchiveKind::Backup, "1.2.3")
        );
        extract(reader, &dest, MAX_DB_SIZE).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"db");

        let mut big = manifest.clone();
        big.app = "x".repeat(MAX_MANIFEST_SIZE as usize + 1);
        assert_eq!(
            read_manifest(&mut &archive(&big, b"db")[..]).unwrap_err(),
            BackupError::Invalid
        );

        let data = archive(&manifest, &[b'd'; 1025]);
        let mut reader = &data[..];
        read_manifest(&mut reader).unwrap();
        assert_eq!(
            extract(reader, &dest, 1024).unwrap_err(),
            BackupError::Invalid,
            "过大的数据库应被拒绝"
        );

        for bad in [
            &b"hello"[..],
            b"termlet-archive/1\n{\"format\":2}\n",
            b"termlet-archive/1\nnot json\n",
        ] {
            assert_eq!(
                read_manifest(&mut &bad[..]).unwrap_err(),
                BackupError::Invalid
            );
        }
        // 数据库部分损坏
        let mut data = archive(&manifest, &[7u8; 4096]);
        let len = data.len();
        data.truncate(len - 20);
        let mut reader = &data[..];
        read_manifest(&mut reader).unwrap();
        assert_eq!(
            extract(reader, &dest, MAX_DB_SIZE).unwrap_err(),
            BackupError::Invalid
        );
    }

    #[test]
    fn data_key_is_hidden_and_round_trips() {
        let mut manifest = Manifest::new(ArchiveKind::Backup, "x");
        let key = DataKey::generate();
        manifest.data_key = Some(key.clone());
        assert!(format!("{manifest:?}").contains("DataKey(***)"));
        let json = serde_json::to_string(&manifest).unwrap();
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data_key.unwrap().0[..], key.0[..]);
        // 快照没有数据密钥
        let snapshot = Manifest::new(ArchiveKind::Snapshot, "x");
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("dataKey")
        );
    }

    #[test]
    fn temp_file_is_removed_with_sqlite_side_files() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = TempFile::new(dir.path());
        let path = tmp.path().to_path_buf();
        std::fs::write(&path, b"x").unwrap();
        let mut journal = path.clone().into_os_string();
        journal.push("-journal");
        std::fs::write(&journal, b"x").unwrap();
        drop(tmp);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
