//! 测试用：模拟一台电脑上的 Termlet（各自的数据目录与保险库主密钥）。备份、导入、主机文件、云同步的测试共用。
//! 通用的辅助（`block_on`、共用的内存钥匙串等）见 `crate::testkit`。

use std::path::PathBuf;
use std::sync::Arc;

use zeroize::Zeroizing;

use super::Backups;
use crate::db::Database;
use crate::id::HostId;
use crate::model::{Host, SecretField};
use crate::secret::SecretString;
use crate::store::{Owner, Position, Store};
use crate::testkit::SharedKeys;
use crate::vault::{KEY_LEN, KeyStore, Vault};
use crate::{CoreError, Result};

/// 钥匙串里没有主密钥、而且写不进去。
pub(crate) struct NoKeyring;

impl KeyStore for NoKeyring {
    fn load(&self) -> Result<Option<Zeroizing<[u8; KEY_LEN]>>> {
        Ok(None)
    }
    fn store(&self, _: &[u8; KEY_LEN]) -> Result<()> {
        Err(CoreError::Keyring("测试：钥匙串不可用".into()))
    }
    fn delete(&self) -> Result<()> {
        Ok(())
    }
}

/// 字段按声明顺序析构：数据库先关、临时目录最后删（Windows 上删不掉打开着的数据库文件，目录会留在临时目录里）。
pub(crate) struct Machine {
    pub store: Arc<Store>,
    pub backups: Backups,
    pub dir: tempfile::TempDir,
}

impl Machine {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self::open(dir, Box::new(SharedKeys::default()))
    }

    fn open(dir: tempfile::TempDir, keys: Box<dyn KeyStore>) -> Self {
        let path = dir.path().join("data.db");
        let db = Database::open(&path, &dir.path().join("backups")).unwrap();
        let vault = Vault::open(&db, keys).unwrap();
        let store = Arc::new(Store::open(db, vault).unwrap());
        let backups = Backups::new(store.clone(), dir.path().join("backups"));
        Self {
            store,
            backups,
            dir,
        }
    }

    /// 用另一个钥匙串重新打开同一个数据目录（模拟把数据目录拷到了另一台电脑）。
    pub fn reopen(self, keys: impl KeyStore + 'static) -> Self {
        let Machine {
            store,
            backups,
            dir,
        } = self;
        drop(backups);
        drop(store);
        Self::open(dir, Box::new(keys))
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn add_host(&self, name: &str, address: &str, password: &str) -> HostId {
        let id = self
            .store
            .create(
                Host {
                    name: name.into(),
                    address: address.into(),
                    username: "root".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        self.set_password(&id, password);
        id
    }

    pub fn set_password(&self, id: &HostId, password: &str) {
        self.store
            .set_secret(
                &Owner::host(id),
                SecretField::Password,
                Some(&SecretString::new(password)),
            )
            .unwrap();
    }

    pub fn password(&self, id: &HostId) -> Option<String> {
        self.store
            .secret(&Owner::host(id), SecretField::Password)
            .unwrap()
            .map(|s| s.expose().to_string())
    }

    pub fn host_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .store
            .list::<Host>()
            .into_iter()
            .map(|h| h.doc.name)
            .collect();
        names.sort();
        names
    }

    pub fn host_named(&self, name: &str) -> crate::store::Record<Host> {
        self.store
            .list::<Host>()
            .into_iter()
            .find(|h| h.doc.name == name)
            .unwrap_or_else(|| panic!("没有主机 {name}"))
    }
}
