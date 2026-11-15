//! 应用级的共享状态：存储，以及把存储的变更事件转成 GPUI 的通知。

use std::sync::Arc;

use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global, Task};
use termlet_core::db::Database;
use termlet_core::paths::DataDir;
use termlet_core::sftp::{ManagerOptions, SftpManager};
use termlet_core::store::{Store, StoreEvent};
use termlet_core::ui_state::UiStateFile;
use termlet_core::vault::{KeyStore, MemoryKeyStore, Vault};

use crate::ui_state::UiStateModel;

/// 存储在 GPUI 中的代表。视图读取数据时经它拿到 [`Store`]，并订阅（`cx.subscribe`）它转发的 [`StoreEvent`]，
/// 或观察（`cx.observe`）它在每次变更后的通知。
pub struct StoreModel {
    store: Arc<Store>,
    _events: Task<()>,
}

impl EventEmitter<StoreEvent> for StoreModel {}

impl StoreModel {
    fn new(store: Arc<Store>, cx: &mut Context<Self>) -> Self {
        let mut events = store.subscribe();
        let task = cx.spawn(async move |this, cx| {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let alive = this.update(cx, |_, cx| {
                            cx.emit(event);
                            cx.notify();
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                    // 事件积压被丢弃时，通知一次让观察者整体重新读取
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self {
            store,
            _events: task,
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }
}

/// 应用级的全局量。
pub struct AppState {
    pub data_dir: DataDir,
    pub store: Entity<StoreModel>,
    /// 只与这台电脑有关的界面状态
    pub ui: Entity<UiStateModel>,
    /// SFTP 的会话与传输（各窗口共用，按窗口区分）
    pub sftp: SftpManager,
    /// 端口转发（管理器与状态快照）
    pub forwards: Entity<crate::forwards::ForwardsModel>,
    /// 完整备份、恢复与本机快照（应用共用一个：同一时间只进行一项）
    pub backups: termlet_core::backup::Backups,
    /// 定时备份（按计划自动创建完整备份）
    pub schedule: termlet_core::backup::BackupSchedule,
    /// 各窗口的询问入口（确认指纹、输入密码），按窗口的标识；窗口打开时登记
    prompters: std::cell::RefCell<std::collections::HashMap<String, termlet_core::ssh::Prompter>>,
    /// UI 测试的数据目录，随应用一起删除
    _test_dir: Option<TestDataDir>,
}

/// UI 测试的数据目录：每个测试一个（备份、快照、日志互不影响），用完删除；以前的运行没删掉的
/// （一小时以前的）在第一次用到时清理。
struct TestDataDir(std::path::PathBuf);

impl TestDataDir {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        static CLEANED: std::sync::Once = std::sync::Once::new();
        let root = std::env::temp_dir().join("termlet-ui-test");
        CLEANED.call_once(|| Self::remove_stale(&root));
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(root.join(format!("{}-{n}", std::process::id())))
    }

    fn remove_stale(root: &std::path::Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        let hour = std::time::Duration::from_secs(60 * 60);
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t.elapsed().unwrap_or_default() > hour);
            if stale {
                let path = entry.path();
                let _ = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
        }
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Global for AppState {}

impl AppState {
    /// 安装全局的存储。
    pub fn install(data_dir: DataDir, store: Arc<Store>, cx: &mut App) {
        let ui_file = UiStateFile::new(data_dir.ui_state());
        Self::install_with(data_dir, store, Some(ui_file), cx);
        // 本机快照：清掉上次异常退出留下的临时文件，按设置定时保存；定时备份按计划自动做
        let backups = cx.global::<AppState>().backups.clone();
        backups.clean_temp();
        backups.spawn_auto();
        cx.global::<AppState>().schedule.spawn_auto();
    }

    fn install_with(
        data_dir: DataDir,
        store: Arc<Store>,
        ui_file: Option<UiStateFile>,
        cx: &mut App,
    ) {
        crate::keymap::apply(&store.settings().keybindings, cx);
        // 快捷键触发的动作（只记动作名；输入框里的编辑动作太多，不记）
        cx.observe_keystrokes(|event, _, _| {
            if let Some(action) = &event.action {
                let name = action.name();
                if !name.starts_with("input::") {
                    tracing::debug!(action = name, "快捷键：执行动作");
                }
            }
        })
        .detach();
        let sftp = SftpManager::new(ManagerOptions::with_store(&store));
        let backups = termlet_core::backup::Backups::new(store.clone(), data_dir.backups_dir());
        let schedule = termlet_core::backup::BackupSchedule::new(backups.clone());
        let forwards = {
            let store = store.clone();
            cx.new(|cx| crate::forwards::ForwardsModel::new(store, cx))
        };
        let store = cx.new(|cx| StoreModel::new(store, cx));
        // 快捷键随设置变化（另一台电脑同步过来的也是）；日志的详细程度、界面大小随本机设置变化，立即生效
        cx.subscribe(&store, |store, event: &StoreEvent, cx| match event {
            StoreEvent::SettingsChanged => {
                let settings = store.read(cx).store().settings();
                crate::keymap::apply(&settings.keybindings, cx);
            }
            StoreEvent::LocalSettingsChanged => {
                let level = store.read(cx).store().local_settings().logs.level;
                termlet_core::logging::apply_setting(level);
                crate::cli_bridge::sync_launch_config(cx);
                crate::ui_scale::apply(cx);
            }
            _ => {}
        })
        .detach();
        let ui = cx.new(|_| UiStateModel::load(ui_file));
        cx.set_global(AppState {
            data_dir,
            store,
            ui,
            sftp,
            forwards,
            backups,
            schedule,
            prompters: Default::default(),
            _test_dir: None,
        });
        crate::sync_ui::install(cx);
        crate::ui_scale::apply(cx);
    }

    /// 使用内存数据库与内存钥匙串（UI 集成测试用）。
    pub fn install_in_memory(cx: &mut App) {
        Self::install_in_memory_with(Box::new(MemoryKeyStore::default()), cx);
    }

    /// 使用内存数据库与指定的钥匙串（UI 集成测试用，例如模拟钥匙串不可用）。
    pub fn install_in_memory_with(keys: Box<dyn KeyStore>, cx: &mut App) {
        Self::install_in_memory_as(keys, false, cx);
    }

    /// 像便携版那样使用内存数据库（UI 集成测试用）：数据目录标为便携版，保险库第一次使用时先让用户选择。
    pub fn install_in_memory_portable(cx: &mut App) {
        Self::install_in_memory_as(Box::new(MemoryKeyStore::default()), true, cx);
    }

    fn install_in_memory_as(keys: Box<dyn KeyStore>, portable: bool, cx: &mut App) {
        let db = Database::open_in_memory().expect("内存数据库");
        let vault = if portable {
            Vault::open_portable(&db, keys)
        } else {
            Vault::open(&db, keys)
        }
        .expect("保险库");
        let store = Store::open(db, vault).expect("存储");
        // 界面语言由测试自己设定：保存设置时不切换（语言是进程全局的，会影响并行的其他测试）
        crate::appearance::assume_language(store.settings().appearance.language, cx);
        // 系统的文字大小按 100%：界面大小不随跑测试的电脑变
        cx.set_global(crate::ui_scale::SystemOverride(
            termlet_core::ui_scale::DEFAULT,
        ));
        let test_dir = TestDataDir::new();
        let dir = DataDir::at(test_dir.0.clone());
        let dir = if portable { dir.into_portable() } else { dir };
        // 测试之间互不影响：界面状态不落盘，数据目录各用各的
        Self::install_with(dir, Arc::new(store), None, cx);
        cx.global_mut::<AppState>()._test_dir = Some(test_dir);
        crate::lock_ui::install_in_memory(cx);
        crate::usage_ui::install_in_memory(cx);
    }

    pub fn global(cx: &App) -> &AppState {
        cx.global::<AppState>()
    }

    /// 是不是便携版（数据在程序旁边，不改 PATH、不自动安装更新）；应用状态还没装好时当作不是。
    pub fn is_portable(cx: &App) -> bool {
        cx.try_global::<AppState>()
            .is_some_and(|app| app.data_dir.is_portable())
    }

    pub fn store(cx: &App) -> Arc<Store> {
        cx.global::<AppState>().store.read(cx).store().clone()
    }

    pub fn ui(cx: &App) -> Entity<UiStateModel> {
        cx.global::<AppState>().ui.clone()
    }

    pub fn sftp(cx: &App) -> SftpManager {
        cx.global::<AppState>().sftp.clone()
    }

    /// 登记窗口的询问入口（窗口打开时）。
    pub fn register_prompter(owner: String, prompter: termlet_core::ssh::Prompter, cx: &App) {
        cx.global::<AppState>()
            .prompters
            .borrow_mut()
            .insert(owner, prompter);
    }

    /// 窗口的询问入口：询问在这个窗口里弹出；没有登记时不询问（需要询问的连接失败）。
    pub fn prompter_for(window: &gpui_kit::Window, cx: &App) -> termlet_core::ssh::Prompter {
        cx.global::<AppState>()
            .prompters
            .borrow()
            .get(&crate::sftp::panel::window_owner(window))
            .cloned()
            .unwrap_or_else(termlet_core::ssh::no_prompt)
    }
}
