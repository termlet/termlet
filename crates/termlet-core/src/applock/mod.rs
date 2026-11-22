//! 应用锁：离开电脑时锁定 Termlet（锁定状态、锁定密码、输错后的等待；系统空闲、系统锁屏时自动锁定，Windows Hello）。
//!
//! - 锁定期间界面被锁屏盖住、终端输入不进会话，`termlet` 命令一律返回 `app_locked`（退出码 6），已打开的会话、
//!   传输与转发在后台继续。
//! - 解锁：锁定密码（启用应用锁时必须设置，作为保底），或系统验证（Windows Hello，见 [`platform`] 的说明）。
//!   威胁模型：防的是“离开时别人使用这台已登录的电脑”；能以同一用户的身份运行程序、读写文件的人本来就能
//!   访问用户的全部文件，应用锁不防御这种情况。
//! - 锁定密码：至少 [`MIN_PASSWORD_LEN`] 个字符，加盐后用 Argon2id（64 MB、3 次、4 路，与主密码相同）哈希，
//!   存在数据库的 `meta` 中（`applock.password`）。`meta` 只属于这台电脑：完整备份不带走（备份时清空 `meta`），
//!   恢复备份与快照时保留本机的，云同步不碰。应用锁的三项设置（空闲多久、启动时、系统锁屏时）在不同步的本机设置
//!   `LocalSettings::app_lock` 中。哈希损坏、认不出时当作没有启用（记一条警告），不会把用户锁在外面。
//! - 连续输错时逐次多等 1 秒（最多 5 秒）才返回，减缓猜密码；解锁成功后清零。修改、停用时验证当前密码也算在内。
//! - 自动锁定（[`AppLock::spawn_auto_lock`]，每 [`AUTO_CHECK_INTERVAL`] 检查一次）：系统空闲达到设置的分钟数、
//!   或系统锁屏（从未锁屏变为锁屏）时锁定。两次检查之间墙上时间过去了 [`SLEEP_GAP`] 以上（睡眠、休眠：合盖后
//!   进程在下一次查询前就被挂起，唤醒后很快解开系统锁屏，两次查询都看不到锁屏）时，开了系统锁屏时锁定的锁定，
//!   空闲设置的分钟数不长于这段时间的同样锁定。启动时锁定：[`AppLock::open`] 按设置以锁定状态开始。
//!
//! # 用法（界面）
//!
//! - 启动时（存储打开后）[`AppLock::open`]，放进 `AppState`（克隆很便宜，各处共用），并调用一次
//!   [`AppLock::spawn_auto_lock`]。命令行服务启动时按 [`AppLock::status`] 调一次 `CliService::set_app_locked`，
//!   之后在 [`AppLock::subscribe`] 的每次变化中再调用（`watch::Receiver::changed` 与执行器无关，可以在 GPUI 的
//!   任务里等待；同一个任务里更新各窗口的锁屏）。
//! - 锁屏：`locked` 时每个窗口都盖上锁屏（盖住对话框、菜单与提示，Esc 关不掉；标题栏仍可拖动），焦点移到密码框；
//!   解锁后焦点回到原来的位置，对话框原样还在。锁定期间界面拒绝会写进会话或替用户作答的操作（打开与重连
//!   会话、打开 SFTP、启动转发、测试连接、终端输入、填入密码、回答询问、选择要上传的文件），询问在超时前继续等待，
//!   解锁后照常回答。
//!   - 密码框 Enter：[`AppLock::unlock`]；[`LockError::WrongPassword`] 时清空密码框、提示“锁定密码错误”（返回前已经
//!     等过输错后的等待，期间按钮显示为忙）。
//!   - [`AppLock::system_available`] 为真时显示“使用 Windows Hello 解锁”，点击后 [`AppLock::unlock_with_system`]
//!     （传入窗口的 HWND 与提示文字“解锁 Termlet”）；[`LockError::System`] 为 [`Verification::Canceled`] 时
//!     不提示，其余提示“系统验证未通过”。
//! - 立即锁定：快捷键 Ctrl+Shift+L、macOS 上 ⇧⌘L（`KeyAction::Lock`，已启用时生效）、命令面板、设置页的按钮，调用
//!   [`AppLock::lock_now`]。
//! - 设置的“安全”页：未启用时“启用应用锁”（新密码输入两次，[`AppLock::set_password`] 的 `current`
//!   为 `None`）；已启用时“修改密码”（当前密码与新密码）、“停用”（[`AppLock::disable`]，要当前密码），以及三项设置：
//!   无操作自动锁定（[`IDLE_CHOICES`]：从不、5、15、30、60 分钟）、启动时锁定、系统锁屏时锁定（经存储的
//!   `update_local_settings` 保存，下一次检查即生效）。新密码过短时为 [`LockError::TooShort`]。
//! - 方法是异步的（哈希在后台线程上算），内部在核心的运行时上执行，返回的 future 是 `Send`，可以在 GPUI 的任务里
//!   直接 `await`；[`AppLock::status`]、[`AppLock::lock_now`] 是同步的。

pub mod platform;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::num::NonZeroIsize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zeroize::Zeroizing;

pub use platform::{LockPlatform, SystemPlatform, Verification};

use crate::model::AppLockSettings;
use crate::secret::SecretString;
use crate::store::Store;
use crate::{CoreError, db};

/// 锁定密码的最短长度（字符）。
pub const MIN_PASSWORD_LEN: usize = 6;
/// 设置中“无操作自动锁定”的选项（分钟，0 表示从不）。
pub const IDLE_CHOICES: &[u32] = &[0, 5, 15, 30, 60];
/// 空闲分钟数的上限（一天）。
pub const MAX_IDLE_MINUTES: u32 = 24 * 60;
/// 自动锁定的检查间隔：系统锁屏也要靠查询得知（见 [`platform`]），间隔短一些。
pub const AUTO_CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// 两次自动锁定检查之间墙上时间过去这么久，就当作系统睡眠过（见模块说明）。
pub const SLEEP_GAP: Duration = Duration::from_secs(30);

/// 存放锁定密码哈希的 `meta` 键。
const META_PASSWORD: &str = "applock.password";
/// 输错后的等待：每次多等一个单位，最多这么多个。
const MAX_DELAY_STEPS: u32 = 5;
/// 检查系统验证是否可用的时限。
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// 等用户完成系统验证的时限。
const VERIFY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// 系统验证的提示文字的长度上限（字符）。
const MAX_MESSAGE_CHARS: usize = 200;

/// 应用锁的状态。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LockStatus {
    /// 已设置锁定密码
    pub enabled: bool,
    pub locked: bool,
}

/// 锁定的原因（写进日志）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockReason {
    Startup,
    Manual,
    Idle,
    SystemLock,
}

/// 应用锁的错误。
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("还没有启用应用锁")]
    NotEnabled,
    #[error("锁定密码错误")]
    WrongPassword,
    #[error("锁定密码至少 6 个字符")]
    TooShort,
    /// 系统验证没有通过（取消、不可用、失败）
    #[error("系统验证未通过（{0:?}）")]
    System(Verification),
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// 应用锁，见模块说明。克隆的是同一个。
#[derive(Clone)]
pub struct AppLock {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AppLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppLock")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// 系统验证是否可用的检查（并发的调用共用一次）：结果与得出结果的时间。
type Availability = Shared<BoxFuture<'static, (bool, Instant)>>;

struct Inner {
    store: Arc<Store>,
    platform: Arc<dyn LockPlatform>,
    status: watch::Sender<LockStatus>,
    /// 连续输错的次数
    failures: AtomicU32,
    /// 设置、停用、解锁依次进行
    ops: tokio::sync::Mutex<()>,
    /// 可用的结果一直有效；不可用的过 [`Inner::unavailable_ttl`] 后重新检查（首次检查超时、被拦截，
    /// 或用户之后才设置 Windows Hello）
    availability: Mutex<Option<Availability>>,
    /// 输错后每次多等的时间
    delay_unit: Duration,
    unavailable_ttl: Duration,
    /// 自动锁定的检查间隔
    auto_interval: Duration,
}

impl AppLock {
    /// 读出是否已启用；已启用且设置了“启动时锁定”时以锁定状态开始。
    pub fn open(store: Arc<Store>) -> Result<Self, LockError> {
        Self::open_with(store, Arc::new(SystemPlatform))
    }

    /// 同 [`AppLock::open`]，系统功能换成 `platform`（测试、UI 测试用）。
    pub fn open_with(
        store: Arc<Store>,
        platform: Arc<dyn LockPlatform>,
    ) -> Result<Self, LockError> {
        let enabled = load_hash(&store)?.is_some();
        let locked = enabled && store.local_settings().app_lock.on_startup;
        if locked {
            tracing::info!(reason = ?LockReason::Startup, "应用已锁定");
        }
        let (delay_unit, unavailable_ttl, auto_interval) = if cfg!(test) {
            (
                Duration::from_millis(1),
                Duration::from_millis(50),
                Duration::from_millis(10),
            )
        } else {
            (
                Duration::from_secs(1),
                Duration::from_secs(60),
                AUTO_CHECK_INTERVAL,
            )
        };
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                platform,
                status: watch::channel(LockStatus { enabled, locked }).0,
                failures: AtomicU32::new(0),
                ops: tokio::sync::Mutex::new(()),
                availability: Mutex::new(None),
                delay_unit,
                unavailable_ttl,
                auto_interval,
            }),
        })
    }

    pub fn status(&self) -> LockStatus {
        *self.inner.status.borrow()
    }

    pub fn is_locked(&self) -> bool {
        self.status().locked
    }

    /// 订阅状态的变化（只在变化时通知）。
    pub fn subscribe(&self) -> watch::Receiver<LockStatus> {
        self.inner.status.subscribe()
    }

    /// 启用应用锁或修改锁定密码。已启用时要提供当前密码（输错时与解锁一样等待）。
    pub async fn set_password(
        &self,
        current: Option<&SecretString>,
        new: &SecretString,
    ) -> Result<(), LockError> {
        if new.expose().chars().count() < MIN_PASSWORD_LEN {
            return Err(LockError::TooShort);
        }
        let inner = self.inner.clone();
        let current = current.cloned().unwrap_or_default();
        let new = new.clone();
        on_runtime(async move {
            let _ops = inner.ops.lock().await;
            let was_enabled = inner.status.borrow().enabled;
            if was_enabled {
                inner.verify(current).await?;
            }
            let hash = blocking(move || PasswordHash::generate(&new)).await??;
            let raw = serde_json::to_vec(&hash).map_err(CoreError::from)?;
            let store = inner.store.clone();
            blocking(move || store.database().set_meta(META_PASSWORD, &raw)).await??;
            inner
                .status
                .send_if_modified(|s| !std::mem::replace(&mut s.enabled, true));
            tracing::info!(
                "应用锁：已{}锁定密码",
                if was_enabled { "修改" } else { "设置" }
            );
            Ok(())
        })
        .await
    }

    /// 停用应用锁（要当前密码），并解除锁定。
    pub async fn disable(&self, password: &SecretString) -> Result<(), LockError> {
        let inner = self.inner.clone();
        let password = password.clone();
        on_runtime(async move {
            let _ops = inner.ops.lock().await;
            if !inner.status.borrow().enabled {
                return Err(LockError::NotEnabled);
            }
            inner.verify(password).await?;
            let store = inner.store.clone();
            blocking(move || {
                store
                    .database()
                    .write(|tx| Ok(db::delete_meta(tx, META_PASSWORD)?))
            })
            .await??;
            inner.failures.store(0, Ordering::Relaxed);
            inner.status.send_replace(LockStatus::default());
            tracing::info!("应用锁：已停用");
            Ok(())
        })
        .await
    }

    /// 立即锁定；没有启用时为 [`LockError::NotEnabled`]。
    pub fn lock_now(&self) -> Result<(), LockError> {
        self.inner.lock(LockReason::Manual)
    }

    /// 用锁定密码解锁。输错时逐次多等一会儿才返回 [`LockError::WrongPassword`]。
    pub async fn unlock(&self, password: &SecretString) -> Result<(), LockError> {
        let inner = self.inner.clone();
        let password = password.clone();
        on_runtime(async move {
            let _ops = inner.ops.lock().await;
            inner.verify(password).await?;
            inner.unlocked("锁定密码");
            Ok(())
        })
        .await
    }

    /// 能否用系统验证（Windows Hello）解锁。可用的结果一直有效，不可用的约 1 分钟后重新检查；并发的调用共用一次检查。
    pub async fn system_available(&self) -> bool {
        let check = {
            let mut cache = self
                .inner
                .availability
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let ttl = self.inner.unavailable_ttl;
            let reusable = cache.as_ref().filter(|check| match check.peek() {
                Some((false, at)) => at.elapsed() < ttl,
                _ => true,
            });
            match reusable {
                Some(check) => check.clone(),
                None => {
                    let check = self.inner.check_availability();
                    *cache = Some(check.clone());
                    check
                }
            }
        };
        check.await.0
    }

    /// 弹出系统验证（`window` 为验证框所属窗口的 HWND，`message` 为提示文字），通过后解锁。
    pub async fn unlock_with_system(
        &self,
        window: Option<NonZeroIsize>,
        message: &str,
    ) -> Result<(), LockError> {
        if !self.status().enabled {
            return Err(LockError::NotEnabled);
        }
        if !self.system_available().await {
            return Err(LockError::System(Verification::Unavailable));
        }
        let inner = self.inner.clone();
        let message: String = message.chars().take(MAX_MESSAGE_CHARS).collect();
        on_runtime(async move {
            let platform = inner.platform.clone();
            let verify =
                tokio::task::spawn_blocking(move || platform.system_verify(window, &message));
            let verdict = match tokio::time::timeout(VERIFY_TIMEOUT, verify).await {
                Ok(Ok(verdict)) => verdict,
                Ok(Err(e)) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                _ => Verification::Failed,
            };
            if verdict != Verification::Verified {
                tracing::info!(?verdict, "应用锁：系统验证没有通过");
                return Err(LockError::System(verdict));
            }
            if !inner.status.borrow().enabled {
                return Err(LockError::NotEnabled);
            }
            inner.unlocked("系统验证");
            Ok(())
        })
        .await
    }

    /// 启动自动锁定的检查（见模块说明），在核心的运行时上进行；所有 [`AppLock`] 都丢弃后自行结束。只调用一次。
    pub fn spawn_auto_lock(&self) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Inner> = Arc::downgrade(&self.inner);
        let interval = self.inner.auto_interval;
        crate::runtime::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // 上一次看到的会话锁屏状态：只在从未锁屏变为锁屏时锁定（查询结果一直是锁屏时不会解锁后马上又锁上）
            let mut was_locked: Option<bool> = None;
            // 上一次检查时的墙上时间（睡眠期间单调时钟不一定走，墙上时间一定走）
            let mut last_check = std::time::SystemTime::now();
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let now = std::time::SystemTime::now();
                let gap = now.duration_since(last_check).unwrap_or_default();
                last_check = now;
                let settings = inner.store.local_settings().app_lock;
                let platform = inner.platform.clone();
                let watch_session = settings.on_system_lock;
                let watch_idle = settings.idle_minutes > 0;
                let probe = tokio::task::spawn_blocking(move || {
                    (
                        watch_idle.then(|| platform.idle_time()).flatten(),
                        watch_session.then(|| platform.session_locked()).flatten(),
                    )
                })
                .await;
                let Ok((idle, session)) = probe else {
                    continue;
                };
                let status = *inner.status.borrow();
                if status.enabled
                    && !status.locked
                    && let Some(reason) = sleep_lock_reason(&settings, gap)
                        .or_else(|| auto_lock_reason(&settings, idle, was_locked, session))
                {
                    let _ = inner.lock(reason);
                }
                if session.is_some() || !watch_session {
                    was_locked = session;
                }
            }
        })
    }
}

impl Inner {
    fn lock(&self, reason: LockReason) -> Result<(), LockError> {
        let mut enabled = true;
        let changed = self.status.send_if_modified(|s| {
            enabled = s.enabled;
            s.enabled && !std::mem::replace(&mut s.locked, true)
        });
        if !enabled {
            return Err(LockError::NotEnabled);
        }
        if changed {
            tracing::info!(?reason, "应用已锁定");
        }
        Ok(())
    }

    fn unlocked(&self, by: &str) {
        self.failures.store(0, Ordering::Relaxed);
        crate::usage::mark(crate::usage::Feature::AppLock);
        if self
            .status
            .send_if_modified(|s| std::mem::replace(&mut s.locked, false))
        {
            tracing::info!("应用已解锁（{by}）");
        }
    }

    /// 验证锁定密码；输错时等一会儿再返回。必须在核心的运行时上调用。
    async fn verify(&self, password: SecretString) -> Result<(), LockError> {
        let store = self.store.clone();
        let hash = blocking(move || load_hash(&store))
            .await??
            .ok_or(LockError::NotEnabled)?;
        if blocking(move || hash.verify(&password)).await?? {
            return Ok(());
        }
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        tokio::time::sleep(self.delay_unit * failures.min(MAX_DELAY_STEPS)).await;
        tracing::info!(failures, "应用锁：锁定密码错误");
        Err(LockError::WrongPassword)
    }

    fn check_availability(&self) -> Availability {
        let platform = self.platform.clone();
        let task = crate::runtime::spawn(async move {
            let check = tokio::task::spawn_blocking(move || platform.system_auth_available());
            let ok = matches!(
                tokio::time::timeout(CHECK_TIMEOUT, check).await,
                Ok(Ok(true))
            );
            (ok, Instant::now())
        });
        async move { task.await.unwrap_or_else(|_| (false, Instant::now())) }
            .boxed()
            .shared()
    }
}

/// 是否该自动锁定（纯函数）：系统空闲达到设置的分钟数，或开启了“系统锁屏时锁定”且会话从未锁屏变为锁屏。
fn auto_lock_reason(
    settings: &AppLockSettings,
    idle: Option<Duration>,
    was_locked: Option<bool>,
    session_locked: Option<bool>,
) -> Option<LockReason> {
    if settings.on_system_lock && was_locked == Some(false) && session_locked == Some(true) {
        return Some(LockReason::SystemLock);
    }
    let minutes = settings.idle_minutes.min(MAX_IDLE_MINUTES);
    if minutes > 0 && idle.is_some_and(|idle| idle >= Duration::from_secs(u64::from(minutes) * 60))
    {
        return Some(LockReason::Idle);
    }
    None
}

/// 两次检查之间过去了 `gap`（墙上时间）：睡眠过时是否锁定（见模块说明）。
fn sleep_lock_reason(settings: &AppLockSettings, gap: Duration) -> Option<LockReason> {
    if gap < SLEEP_GAP {
        return None;
    }
    if settings.on_system_lock {
        return Some(LockReason::SystemLock);
    }
    let minutes = settings.idle_minutes.min(MAX_IDLE_MINUTES);
    (minutes > 0 && gap >= Duration::from_secs(u64::from(minutes) * 60)).then_some(LockReason::Idle)
}

// ---------------------------------------------------------------- 锁定密码的哈希

/// 存在 `meta` 中的锁定密码哈希。
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasswordHash {
    v: u32,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    /// Base64
    salt: String,
    /// Base64，32 字节
    hash: String,
}

const HASH_LEN: usize = 32;
/// Argon2id 的内存（KiB）、迭代次数、并行度：与主密码相同（64 MB、3 次、4 路）。
const ARGON2_PARAMS: (u32, u32, u32) = (64 * 1024, 3, 4);

impl PasswordHash {
    /// 生成新的哈希（随机的 16 字节盐，[`ARGON2_PARAMS`]；测试中用小参数）。
    fn generate(password: &SecretString) -> Result<Self, CoreError> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt)
            .map_err(|e| CoreError::DataDir(format!("系统随机数不可用：{e}")))?;
        let (memory_kib, iterations, parallelism) = if cfg!(test) {
            (64, 1, 1)
        } else {
            ARGON2_PARAMS
        };
        let key = crate::vault::derive_key(
            password.expose().as_bytes(),
            &salt,
            memory_kib,
            iterations,
            parallelism,
        )?;
        Ok(Self {
            v: 1,
            memory_kib,
            iterations,
            parallelism,
            salt: data_encoding::BASE64.encode(&salt),
            hash: data_encoding::BASE64.encode(&key[..]),
        })
    }

    fn verify(&self, password: &SecretString) -> Result<bool, CoreError> {
        let decode = |s: &str| {
            data_encoding::BASE64
                .decode(s.as_bytes())
                .map(Zeroizing::new)
                .map_err(|_| CoreError::DataDir("锁定密码的哈希已损坏".into()))
        };
        let salt = decode(&self.salt)?;
        let expected = decode(&self.hash)?;
        let key = crate::vault::derive_key(
            password.expose().as_bytes(),
            &salt,
            self.memory_kib,
            self.iterations,
            self.parallelism,
        )?;
        Ok(constant_time_eq(&key[..], &expected))
    }

    /// 参数在合理范围内（防止损坏的数据让 Argon2 申请巨量内存）。
    fn is_sane(&self) -> bool {
        let decoded_len = |s: &str| data_encoding::BASE64.decode(s.as_bytes()).map(|v| v.len());
        self.v == 1
            && (8..=1 << 21).contains(&self.memory_kib)
            && (1..=16).contains(&self.iterations)
            && (1..=16).contains(&self.parallelism)
            && decoded_len(&self.salt).is_ok_and(|n| n >= 8)
            && decoded_len(&self.hash) == Ok(HASH_LEN)
    }
}

/// 读出锁定密码的哈希；没有设置时为 `None`。内容损坏、认不出时也当作没有设置（记一条警告）。
fn load_hash(store: &Store) -> Result<Option<PasswordHash>, CoreError> {
    let Some(raw) = store.database().meta(META_PASSWORD)? else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    match serde_json::from_slice::<PasswordHash>(&raw) {
        Ok(hash) if hash.is_sane() => Ok(Some(hash)),
        _ => {
            tracing::warn!("应用锁的密码数据无法识别，当作没有启用应用锁");
            Ok(None)
        }
    }
}

/// 逐字节比较，耗时与内容无关。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------- 运行时

/// 在核心的运行时上执行（等待、计时都要在 tokio 的运行时上），返回的 future 与执行器无关。
async fn on_runtime<T: Send + 'static>(
    future: impl Future<Output = Result<T, LockError>> + Send + 'static,
) -> Result<T, LockError> {
    match crate::runtime::spawn(future).await {
        Ok(result) => result,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(CoreError::Io(std::io::Error::other(e)).into()),
    }
}

/// 在后台线程上执行阻塞的操作（哈希、数据库）。必须在核心的运行时上调用。
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, LockError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Ok(value),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(CoreError::Io(std::io::Error::other(e)).into()),
    }
}
