//! 云同步的测试。

mod dav;
mod detected;
mod gist;
mod merge;
mod s3;
mod safety;
mod sync;
mod webdav;

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::Value;

use super::backend::{Backend, Fetch, Stored};
use super::document::{self, Document, FILE_NAME, MAX_DOCUMENT_SIZE};
use super::records::{Data, Remote, version};
use super::{
    Answer, CloudSync, RemoteInfo, SetupInput, SyncError, SyncOutcome, SyncReport, SyncTarget,
};
use crate::backup::testkit::Machine;
use crate::id::{ForwardId, GroupId, HostId, KeyId, ProfileId};
use crate::model::{Forward, Group, Host, KeyEntry, Profile, ProfileAuth, SecretField};
use crate::secret::SecretString;
use crate::store::sync::{RawChange, RawItem};
use crate::store::{Owner, Position, Store};
use crate::testkit::{SharedKeys, block_on};
use crate::vault::VaultStatus;

/// 测试中替换行为的钩子。
#[derive(Default)]
pub(crate) struct Hooks {
    /// 包装同步位置（模拟其他设备在读写之间写入）
    pub wrap: Option<Arc<dyn Fn(Arc<dyn Backend>) -> Arc<dyn Backend> + Send + Sync>>,
    /// 在读取本机数据之后、写入云端的变更之前调用一次（模拟用户在同步期间修改数据）
    pub before_apply: Option<Box<dyn FnOnce() + Send>>,
    /// 代替应用的版本号（模拟升级）
    pub app: Option<String>,
    /// 代替 GitHub API 的地址（假的 Gist 服务）
    pub gist_api: Option<String>,
}

impl CloudSync {
    pub(crate) fn hooks(&self) -> std::sync::MutexGuard<'_, Hooks> {
        self.inner.hooks.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn before_apply_hook(&self) {
        let hook = self.hooks().before_apply.take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

pub(crate) const PASSWORD: &str = "sync-pass";

/// 模拟一台设备上的 Termlet：各自的数据目录、保险库主密钥与同步服务。
/// `sync` 也持有数据库，排在前面先析构，`m` 才删得掉临时目录（见 [`Machine`]）。
pub(crate) struct Dev {
    pub sync: CloudSync,
    pub m: Machine,
}

impl Dev {
    pub fn new() -> Self {
        Self::of(Machine::new())
    }

    fn of(m: Machine) -> Self {
        let sync = CloudSync::new(m.store.clone(), Some(m.backups.clone()));
        Self { m, sync }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.m.store
    }

    pub fn input(dir: &Path, password: &str) -> SetupInput {
        SetupInput {
            target: SyncTarget::Folder { path: dir.into() },
            credential: SecretString::default(),
            password: SecretString::new(password),
            device_name: "dev".into(),
        }
    }

    pub fn setup(&self, dir: &Path) -> RemoteInfo {
        block_on(self.sync.setup(Self::input(dir, PASSWORD))).unwrap()
    }

    pub fn try_run(&self) -> Result<SyncOutcome, SyncError> {
        block_on(self.sync.sync_now())
    }

    pub fn run(&self) -> SyncOutcome {
        self.try_run().unwrap()
    }

    /// 同步一次，应当完成（不需要用户决定）。
    pub fn sync(&self) -> SyncReport {
        match self.run() {
            SyncOutcome::Done(report) => {
                // 使用统计的功能名单记下同步成功过
                assert!(crate::usage::features::global().contains(crate::usage::Feature::Sync));
                report
            }
            other => panic!("应当同步完成：{other:?}"),
        }
    }

    pub fn answer(&self, answer: Answer) -> SyncOutcome {
        block_on(self.sync.answer(answer)).unwrap()
    }

    pub fn answer_done(&self, answer: Answer) -> SyncReport {
        match self.answer(answer) {
            SyncOutcome::Done(report) => report,
            other => panic!("应当同步完成：{other:?}"),
        }
    }

    pub fn add_host(&self, name: &str, address: &str, password: &str) -> HostId {
        self.m.add_host(name, address, password)
    }

    pub fn names(&self) -> Vec<String> {
        self.m.host_names()
    }

    pub fn password(&self, id: &HostId) -> Option<String> {
        self.m.password(id)
    }

    pub fn host(&self, id: &HostId) -> crate::store::Record<Host> {
        self.store().get::<Host>(id).expect("主机应存在")
    }

    pub fn first_host(&self) -> HostId {
        self.store().list::<Host>()[0].id.clone()
    }

    /// 修改主机（名称）。
    pub fn edit_host(&self, id: &HostId, edit: impl FnOnce(&mut Host)) {
        let mut doc = self.host(id).doc;
        edit(&mut doc);
        self.store().update(id, doc).unwrap();
    }

    pub fn rename_host(&self, id: &HostId, name: &str) {
        self.edit_host(id, |h| h.name = name.into());
    }

    pub fn delete_host(&self, id: &HostId) {
        self.store().delete::<Host>(id).unwrap();
    }

    pub fn forwards(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .store()
            .list::<Forward>()
            .into_iter()
            .map(|f| f.doc.name)
            .collect();
        names.sort();
        names
    }

    /// 把条目（记录键 `host/h_…`）的修改时间改为 `at`，内容不变（控制冲突时哪一边较新）。
    pub fn set_updated(&self, key: &str, at: i64) {
        let snapshot = self.store().sync_snapshot().unwrap();
        let item = snapshot
            .items
            .into_iter()
            .find(|i| format!("{}/{}", i.kind.name(), i.id) == key)
            .expect("条目应存在");
        self.put_item(RawItem {
            updated_at: at,
            ..item
        });
    }

    /// 直接写入一条条目（指定修改时间）。
    pub fn put_item(&self, item: RawItem) {
        self.store()
            .sync_apply(|_| true, &[RawChange::PutItem(item)])
            .unwrap();
    }

    /// 在 `at` 时修改一组设置。
    pub fn edit_setting_at(
        &self,
        group: &str,
        at: i64,
        edit: impl FnOnce(&mut serde_json::Map<String, Value>),
    ) {
        let snapshot = self.store().sync_snapshot().unwrap();
        let setting = snapshot
            .settings
            .into_iter()
            .find(|s| s.id == group)
            .unwrap();
        let Value::Object(mut doc) = setting.doc else {
            panic!("设置应是对象")
        };
        edit(&mut doc);
        self.store()
            .sync_apply(
                |_| true,
                &[RawChange::PutSetting {
                    id: group.into(),
                    doc: Value::Object(doc),
                    updated_at: at,
                    keep_local: &[],
                }],
            )
            .unwrap();
    }

    /// 模拟主密钥丢失（钥匙串中没有原来的主密钥）后重置保险库：秘密清空，同步密码解不开，需要重新设置同步。
    pub fn reset_vault(self) -> Self {
        let Dev { m, sync } = self;
        drop(sync);
        let m = m.reopen(SharedKeys::default());
        assert_eq!(m.store.vault_status(), VaultStatus::KeyMissing);
        m.store.reset_vault(None).unwrap();
        Self::of(m)
    }
}

/// 同步文件夹。
pub(crate) fn sync_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

pub(crate) fn read_doc(dir: &Path) -> Document {
    let data = std::fs::read(dir.join(FILE_NAME)).unwrap();
    document::open(&data, &SecretString::new(PASSWORD), MAX_DOCUMENT_SIZE).unwrap()
}

pub(crate) fn write_doc(dir: &Path, doc: &Document) {
    let sealed = document::seal(
        doc,
        &SecretString::new(PASSWORD),
        "test",
        crate::time::now_ms(),
        MAX_DOCUMENT_SIZE,
    )
    .unwrap();
    std::fs::write(dir.join(FILE_NAME), sealed).unwrap();
}

/// 模拟其他设备修改了云端的一条记录（版本较新）。
pub(crate) fn put_record(doc: &mut Document, key: &str, data: &Value) {
    let data = Data::of(data);
    let v = version(crate::time::now_ms() + 60_000, "other0device", &data.hash());
    doc.records.insert(key.into(), Remote::new(v, data));
}

pub(crate) fn record_value(doc: &Document, key: &str) -> Value {
    doc.records
        .get(key)
        .unwrap_or_else(|| panic!("云端应有 {key}"))
        .data
        .value()
        .unwrap()
}

pub(crate) fn host_key(id: &HostId) -> String {
    format!("host/{id}")
}

// ---- 在存储中加分组、密钥、凭据档案、端口转发（safety 与 sync 的测试共用）

pub(crate) const PRIVATE_KEY: &str =
    "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA测试\n-----END OPENSSH PRIVATE KEY-----";

pub(crate) fn add_key(store: &Store, name: &str) -> KeyId {
    let key = store
        .create(
            KeyEntry {
                name: name.into(),
                algorithm: "ssh-ed25519".into(),
                public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAItest test".into(),
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::key(&key),
            SecretField::PrivateKey,
            Some(&PRIVATE_KEY.into()),
        )
        .unwrap();
    key
}

pub(crate) fn add_profile(store: &Store, name: &str, password: &str) -> ProfileId {
    let profile = store
        .create(
            Profile {
                name: name.into(),
                username: "ops".into(),
                auth: ProfileAuth::Password,
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap();
    store
        .set_secret(
            &Owner::profile(&profile),
            SecretField::Password,
            Some(&password.into()),
        )
        .unwrap();
    profile
}

pub(crate) fn add_group(store: &Store, name: &str, parent: Option<&GroupId>) -> GroupId {
    store
        .create(
            Group {
                name: name.into(),
                ..Default::default()
            },
            parent.map(|p| p.to_string()),
            Position::Last,
        )
        .unwrap()
}

pub(crate) fn add_forward(store: &Store, host: &HostId, name: &str, port: u16) -> ForwardId {
    store
        .create(
            Forward {
                host: Some(host.clone()),
                name: name.into(),
                bind_port: port,
                target_host: "127.0.0.1".into(),
                target_port: 80,
                ..Default::default()
            },
            None,
            Position::Last,
        )
        .unwrap()
}

/// 两台已同步过一次的设备（a 上有一台主机 h1）。
pub(crate) fn pair() -> (Dev, Dev, tempfile::TempDir) {
    let dir = sync_dir();
    let (a, b) = (Dev::new(), Dev::new());
    a.add_host("h1", "10.0.0.1", "p1");
    a.setup(dir.path());
    a.sync();
    b.setup(dir.path());
    b.sync();
    (a, b, dir)
}

/// 在第一次写入前先执行 `before`（模拟另一台设备在这期间先同步），之后照常。
pub(crate) struct Race {
    inner: Arc<dyn Backend>,
    before: Arc<Mutex<Option<BoxFuture<'static, ()>>>>,
}

impl Backend for Race {
    fn get(&self) -> BoxFuture<'_, Result<Fetch, SyncError>> {
        self.inner.get()
    }

    fn put<'a>(
        &'a self,
        data: &'a [u8],
        etag: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Stored, SyncError>> {
        Box::pin(async move {
            let before = self.before.lock().unwrap().take();
            if let Some(before) = before {
                before.await;
            }
            self.inner.put(data, etag).await
        })
    }

    fn check(&self) -> BoxFuture<'_, Result<(), SyncError>> {
        self.inner.check()
    }
}

/// 让 `sync` 第一次写入云端之前先执行 `before`。
pub(crate) fn race_before_put(sync: &CloudSync, before: BoxFuture<'static, ()>) {
    let slot = Arc::new(Mutex::new(Some(before)));
    sync.hooks().wrap = Some(Arc::new(move |inner| {
        Arc::new(Race {
            inner,
            before: slot.clone(),
        }) as Arc<dyn Backend>
    }));
}
