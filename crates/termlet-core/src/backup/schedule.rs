//! 定时备份：按计划（每天或每周的某个时间，本地时间）自动创建完整备份，存到指定的文件夹（可以是网盘的同步目录），
//! 保留最近 N 份。
//!
//! - 备份与手动的完整备份相同（[`super::Backups::create_backup`]，用备份密码加密，不含审计日志），文件名为
//!   `termlet-auto-20261105-030000.termlet-backup`（本地时间）。清理旧的只删文件夹中符合这个规则的文件，手动保存的
//!   备份（`termlet-backup-…`）与其他文件都不动。
//! - 应用运行时到点备份（每分钟检查一次）；错过的（到点时没有运行、电脑睡眠）在下次启动约 2 分钟后补做一次，
//!   错过多次也只补一次。设置或更改计划之前的时间点不补做。每个时间点只尝试一次：失败了记下原因（界面显示），
//!   等下一个时间点或用户点“立即备份”。保险库锁定（等待主密码）时等到解锁后再做，不算失败。
//! - 备份密码至少 [`super::MIN_PASSWORD_LEN`] 个字符（与手动备份相同，完整备份总是加密的），经保险库加密后存在本机
//!   （`meta` 表），不以明文进数据库，不进日志。保险库重置后解不开时停止定时备份，请用户重新输入
//!   （[`ScheduleStatus::password_lost`]）。
//! - 设置、密码与上次的结果都存在 `meta` 表中：属于这台电脑，不进完整备份，恢复备份、快照时也不被替换，不同步。
//!
//! | 键 | 内容 |
//! |---|---|
//! | `backup.schedule` | [`ScheduleConfig`]（JSON） |
//! | `backup.schedule.secret` | 备份密码，经保险库加密 |
//! | `backup.schedule.last` | 上次的结果 [`RunRecord`]（JSON） |

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike as _, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use super::{BackupError, BackupOptions, Backups, envelope};
use crate::CoreError;
use crate::db;
use crate::secret::SecretString;

/// 保留份数的范围。
pub const KEEP_RANGE: RangeInclusive<u32> = 1..=100;
/// 默认保留的份数。
pub const DEFAULT_KEEP: u32 = 7;
/// 定时备份的文件名前缀与扩展名（清理时只认这样的文件）。
pub const FILE_PREFIX: &str = "termlet-auto-";
pub const FILE_EXT: &str = ".termlet-backup";

/// 启动后第一次检查的延迟（补做错过的备份），之后每分钟检查一次。
const FIRST_CHECK: Duration = Duration::from_secs(120);
const CHECK_EVERY: Duration = Duration::from_secs(60);

const META_CONFIG: &str = "backup.schedule";
const META_SECRET: &str = "backup.schedule.secret";
const META_LAST: &str = "backup.schedule.last";
/// 加密备份密码时的附加数据。
const SECRET_AAD: &[u8] = b"termlet:backup-schedule:v1:password";

/// 多久备份一次。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "every", rename_all = "lowercase")]
pub enum Frequency {
    Daily,
    /// 每周的星期几：1 为星期一 … 7 为星期日
    Weekly {
        weekday: u8,
    },
}

/// 定时备份的设置（不含密码）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleConfig {
    /// 存放备份的文件夹（绝对路径）
    pub folder: PathBuf,
    pub frequency: Frequency,
    /// 当天的第几分钟（本地时间，0–1439）
    pub minute_of_day: u16,
    /// 保留最近几份（见 [`KEEP_RANGE`]）
    pub keep: u32,
    /// 设置（或更改计划）的时间（Unix 毫秒）：这之前的时间点不补做
    pub since: i64,
}

/// 设置定时备份的输入。
#[derive(Clone, Debug)]
pub struct ScheduleInput {
    pub folder: PathBuf,
    pub frequency: Frequency,
    pub minute_of_day: u16,
    pub keep: u32,
    /// 备份密码；更改设置时为 `None` 表示沿用原来的
    pub password: Option<SecretString>,
}

/// 一次定时备份（或“立即备份”）的结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
    /// 开始的时间（Unix 毫秒）
    pub at: i64,
    /// 备份的文件名（成功时）
    #[serde(default)]
    pub file: Option<String>,
    /// 失败的原因
    #[serde(default)]
    pub error: Option<BackupError>,
    /// 按保留份数删掉的旧备份数
    #[serde(default)]
    pub deleted: usize,
    /// 用户点“立即备份”做的
    #[serde(default)]
    pub manual: bool,
}

impl RunRecord {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// 定时备份的状态（[`BackupSchedule::subscribe`]）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScheduleStatus {
    /// `None` 表示没有设置定时备份
    pub config: Option<ScheduleConfig>,
    pub running: bool,
    pub last: Option<RunRecord>,
    /// 下一次备份的时间（Unix 毫秒）；错过了、等着补做时为现在
    pub next: Option<i64>,
    /// 保险库重置过，备份密码已解不开：定时备份暂停，需要重新输入
    pub password_lost: bool,
}

// ---------------------------------------------------------------- 计划（纯函数）

/// 某天的计划时间点（本地时间转成 Unix 毫秒）。夏令时跳过的时刻顺延一小时，重复的时刻取前一个。
fn slot_on<Tz: TimeZone>(tz: &Tz, date: NaiveDate, minute_of_day: u16) -> Option<i64> {
    let time = NaiveTime::from_hms_opt(
        u32::from(minute_of_day / 60),
        u32::from(minute_of_day % 60),
        0,
    )?;
    let local = date.and_time(time);
    tz.from_local_datetime(&local)
        .earliest()
        .or_else(|| {
            tz.from_local_datetime(&(local + chrono::Duration::hours(1)))
                .earliest()
        })
        .map(|t| t.timestamp_millis())
}

fn matches_day(frequency: Frequency, date: NaiveDate) -> bool {
    match frequency {
        Frequency::Daily => true,
        Frequency::Weekly { weekday } => date.weekday().number_from_monday() == u32::from(weekday),
    }
}

fn local_date<Tz: TimeZone>(tz: &Tz, ms: i64) -> Option<NaiveDate> {
    tz.timestamp_millis_opt(ms)
        .single()
        .map(|t| t.naive_local().date())
}

/// 最近一个不晚于 `now` 的计划时间点。
pub(crate) fn last_slot<Tz: TimeZone>(
    tz: &Tz,
    frequency: Frequency,
    minute_of_day: u16,
    now: i64,
) -> Option<i64> {
    let today = local_date(tz, now)?;
    (0..=8)
        .filter_map(|back| today.checked_sub_days(chrono::Days::new(back)))
        .filter(|date| matches_day(frequency, *date))
        .filter_map(|date| slot_on(tz, date, minute_of_day))
        .find(|slot| *slot <= now)
}

/// 下一个晚于 `now` 的计划时间点。
pub(crate) fn next_slot<Tz: TimeZone>(
    tz: &Tz,
    frequency: Frequency,
    minute_of_day: u16,
    now: i64,
) -> Option<i64> {
    let today = local_date(tz, now)?;
    (0..=8)
        .filter_map(|ahead| today.checked_add_days(chrono::Days::new(ahead)))
        .filter(|date| matches_day(frequency, *date))
        .filter_map(|date| slot_on(tz, date, minute_of_day))
        .find(|slot| *slot > now)
}

/// 现在是否该备份：最近的计划时间点在设置之后，而且那之后还没有备份过（成功、失败都算做过）。
pub(crate) fn is_due<Tz: TimeZone>(
    tz: &Tz,
    config: &ScheduleConfig,
    last_attempt: Option<i64>,
    now: i64,
) -> bool {
    let Some(slot) = last_slot(tz, config.frequency, config.minute_of_day, now) else {
        return false;
    };
    slot > config.since && last_attempt.is_none_or(|at| at < slot)
}

/// 下一次备份的时间：该补做时为 `now`。
pub(crate) fn next_run<Tz: TimeZone>(
    tz: &Tz,
    config: &ScheduleConfig,
    last_attempt: Option<i64>,
    now: i64,
) -> Option<i64> {
    if is_due(tz, config, last_attempt, now) {
        return Some(now);
    }
    next_slot(tz, config.frequency, config.minute_of_day, now)
}

// ---------------------------------------------------------------- 文件

/// 这次备份的文件名（本地时间）。
pub(crate) fn file_name<Tz: TimeZone>(tz: &Tz, now: i64) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let stamp = tz
        .timestamp_millis_opt(now)
        .single()
        .map(|t| t.format("%Y%m%d-%H%M%S").to_string())
        .unwrap_or_else(|| now.to_string());
    format!("{FILE_PREFIX}{stamp}{FILE_EXT}")
}

/// 是定时备份写的文件：返回文件名中的时间（`YYYYMMDDhhmmss`，用于排序）。
pub(crate) fn parse_file_name(name: &str) -> Option<u64> {
    let stamp = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_EXT)?;
    let (date, time) = stamp.split_once('-')?;
    if date.len() != 8
        || time.len() != 6
        || !date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit())
    {
        return None;
    }
    format!("{date}{time}").parse().ok()
}

/// 按保留份数删除文件夹中较旧的定时备份（只删符合文件名规则的普通文件）。返回删除的份数。
pub(crate) fn prune(folder: &Path, keep: usize) -> std::io::Result<usize> {
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let Some(stamp) = entry.file_name().to_str().and_then(parse_file_name) else {
            continue;
        };
        // 不跟随符号链接：只删真正的文件
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        found.push((stamp, entry.path()));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut deleted = 0;
    for (_, path) in found.into_iter().skip(keep.max(1)) {
        match std::fs::remove_file(&path) {
            Ok(()) => deleted += 1,
            Err(e) => tracing::warn!(file = %path.display(), "删除旧的定时备份失败：{e}"),
        }
    }
    Ok(deleted)
}

/// 检查备份文件夹：绝对路径、存在、可以写入。
fn check_folder(folder: &Path) -> Result<(), BackupError> {
    if folder.as_os_str().is_empty() || !folder.is_absolute() {
        return Err(BackupError::NotAbsolute);
    }
    if !folder.is_dir() {
        return Err(BackupError::FolderMissing {
            path: folder.to_path_buf(),
        });
    }
    let probe = folder.join(format!(
        ".termlet-backup-check-{}",
        crate::id::random_token(5)
    ));
    let written = std::fs::write(&probe, b"ok");
    let _ = std::fs::remove_file(&probe);
    written.map_err(|e| super::write_error(folder, &e))
}

fn validate(input: &ScheduleInput) -> Result<(), BackupError> {
    let weekday_ok = match input.frequency {
        Frequency::Daily => true,
        Frequency::Weekly { weekday } => (1..=7).contains(&weekday),
    };
    if !weekday_ok || input.minute_of_day >= 24 * 60 || !KEEP_RANGE.contains(&input.keep) {
        return Err(BackupError::InvalidSchedule);
    }
    if let Some(password) = &input.password {
        envelope::check_password(password)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- 服务

/// 定时备份。克隆很便宜，各处共用同一个。
#[derive(Clone)]
pub struct BackupSchedule {
    inner: Arc<Inner>,
}

struct Inner {
    backups: Backups,
    /// 同一时间只进行一项（备份、设置、停止）
    lock: tokio::sync::Mutex<()>,
    status: watch::Sender<ScheduleStatus>,
    /// 设置变了：自动备份立即重新检查
    wake: Notify,
}

fn read_json<T: serde::de::DeserializeOwned>(db: &db::Database, key: &str) -> Option<T> {
    db.meta(key)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, BackupError> + Send + 'static,
) -> Result<T, BackupError> {
    super::blocking(f).await
}

impl BackupSchedule {
    /// 读出本机的定时备份设置与上次的结果（只读几个 `meta` 键）。
    pub fn new(backups: Backups) -> Self {
        let db = backups.store().database();
        let config: Option<ScheduleConfig> = read_json(db, META_CONFIG);
        let last: Option<RunRecord> = read_json(db, META_LAST);
        let this = Self {
            inner: Arc::new(Inner {
                backups,
                lock: tokio::sync::Mutex::new(()),
                status: watch::Sender::new(ScheduleStatus {
                    config,
                    last,
                    ..Default::default()
                }),
                wake: Notify::new(),
            }),
        };
        this.publish(|_| {});
        this
    }

    pub fn subscribe(&self) -> watch::Receiver<ScheduleStatus> {
        self.inner.status.subscribe()
    }

    pub fn status(&self) -> ScheduleStatus {
        self.inner.status.borrow().clone()
    }

    fn store(&self) -> &Arc<crate::store::Store> {
        self.inner.backups.store()
    }

    /// 修改状态并重新计算下一次的时间；有变化时通知订阅者（每分钟的检查不会无故让界面重画）。
    fn publish(&self, edit: impl FnOnce(&mut ScheduleStatus)) {
        let now = crate::time::now_ms();
        self.inner.status.send_if_modified(|status| {
            let before = status.clone();
            edit(status);
            status.next = status.config.as_ref().and_then(|config| {
                let last = status.last.as_ref().map(|r| r.at);
                next_run(&chrono::Local, config, last, now)
            });
            *status != before
        });
    }

    /// 设置（或更改）定时备份。检查文件夹（存在、可写）、计划与密码；密码为 `None` 时沿用原来的（没有时报告密码太短）。
    pub async fn configure(&self, input: ScheduleInput) -> Result<ScheduleConfig, BackupError> {
        validate(&input)?;
        let _guard = self.inner.lock.lock().await;
        let store = self.store().clone();
        let folder = input.folder.clone();
        let old = self.status().config;
        let new_password = input.password.is_some();
        let config = blocking(move || {
            check_folder(&folder)?;
            let db = store.database();
            let sealed = match &input.password {
                Some(password) => Some(
                    store
                        .vault()
                        .seal(SECRET_AAD, password.expose().as_bytes())?,
                ),
                None if db.meta(META_SECRET)?.is_some() => None,
                None => {
                    return Err(BackupError::PasswordTooShort {
                        min: envelope::MIN_PASSWORD_LEN,
                    });
                }
            };
            // 计划没变时保留原来的起点（只改文件夹、份数、密码不影响补做）
            let since = match &old {
                Some(old)
                    if old.frequency == input.frequency
                        && old.minute_of_day == input.minute_of_day =>
                {
                    old.since
                }
                _ => crate::time::now_ms(),
            };
            let config = ScheduleConfig {
                folder: input.folder.clone(),
                frequency: input.frequency,
                minute_of_day: input.minute_of_day,
                keep: input.keep,
                since,
            };
            let raw = serde_json::to_vec(&config).map_err(|e| BackupError::Store(e.to_string()))?;
            db.write(|tx| {
                db::set_meta(tx, META_CONFIG, &raw)?;
                if let Some(sealed) = &sealed {
                    db::set_meta(tx, META_SECRET, sealed)?;
                }
                Ok(())
            })?;
            Ok(config)
        })
        .await?;
        let lost = !new_password && self.status().password_lost;
        self.publish(|s| {
            s.config = Some(config.clone());
            s.password_lost = lost;
        });
        self.inner.wake.notify_one();
        tracing::info!(keep = config.keep, "已设置定时备份");
        Ok(config)
    }

    /// 停止定时备份：删除设置、密码与上次的结果。已有的备份文件不动。
    pub async fn disable(&self) -> Result<(), BackupError> {
        let _guard = self.inner.lock.lock().await;
        let store = self.store().clone();
        blocking(move || {
            store.database().write(|tx| {
                for key in [META_CONFIG, META_SECRET, META_LAST] {
                    db::delete_meta(tx, key)?;
                }
                Ok(())
            })?;
            Ok(())
        })
        .await?;
        self.publish(|s| *s = ScheduleStatus::default());
        tracing::info!("已停止定时备份");
        Ok(())
    }

    /// 立即备份一次（按设置的文件夹、密码与保留份数）。结果同时记为上次的结果。
    pub async fn run_now(&self) -> Result<RunRecord, BackupError> {
        let _guard = self.inner.lock.lock().await;
        self.run_locked(true).await
    }

    /// 读出备份密码。
    fn password(&self) -> Result<SecretString, BackupError> {
        let store = self.store();
        let blob = store
            .database()
            .meta(META_SECRET)?
            .ok_or(BackupError::NotScheduled)?;
        match store.vault().open_sealed(SECRET_AAD, &blob) {
            Ok(plain) => Ok(SecretString::new(
                String::from_utf8(plain.to_vec()).map_err(|_| BackupError::SchedulePasswordLost)?,
            )),
            Err(CoreError::Decrypt) => Err(BackupError::SchedulePasswordLost),
            Err(e) => Err(e.into()),
        }
    }

    async fn run_locked(&self, manual: bool) -> Result<RunRecord, BackupError> {
        let config = self.status().config.ok_or(BackupError::NotScheduled)?;
        // 保险库锁定时不算一次备份（自动备份等解锁后再做）
        if !self.store().vault().is_unlocked() {
            return Err(BackupError::VaultLocked);
        }
        let this = self.clone();
        let password = match blocking(move || this.password()).await {
            Err(BackupError::SchedulePasswordLost) => {
                tracing::warn!("保险库重置过，定时备份的密码已无法解开：暂停定时备份");
                self.publish(|s| s.password_lost = true);
                return Err(BackupError::SchedulePasswordLost);
            }
            other => other?,
        };
        self.publish(|s| s.running = true);
        let at = crate::time::now_ms();
        let mut record = RunRecord {
            at,
            file: None,
            error: None,
            deleted: 0,
            manual,
        };
        let name = file_name(&chrono::Local, at);
        let folder = config.folder.clone();
        let result = match blocking(move || check_folder(&folder)).await {
            Ok(()) => {
                self.inner
                    .backups
                    .create_backup(
                        config.folder.join(&name),
                        password,
                        BackupOptions::default(),
                    )
                    .await
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(_) => {
                record.file = Some(name);
                let folder = config.folder.clone();
                let keep = config.keep as usize;
                match blocking(move || {
                    prune(&folder, keep).map_err(|e| super::read_error(&folder, &e))
                })
                .await
                {
                    Ok(deleted) => record.deleted = deleted,
                    Err(e) => tracing::warn!("清理旧的定时备份失败：{e}"),
                }
                tracing::info!(manual, deleted = record.deleted, "定时备份完成");
            }
            Err(e) => {
                tracing::warn!(manual, "定时备份失败：{e}");
                record.error = Some(e);
            }
        }
        let raw = serde_json::to_vec(&record).map_err(|e| BackupError::Store(e.to_string()))?;
        let store = self.store().clone();
        if let Err(e) = blocking(move || Ok(store.database().set_meta(META_LAST, &raw)?)).await {
            tracing::warn!("保存定时备份的结果失败：{e}");
        }
        self.publish(|s| {
            s.running = false;
            s.last = Some(record.clone());
            s.password_lost = false;
        });
        Ok(record)
    }

    /// 到点（或错过了）时备份一次。返回是否做了。
    pub(crate) async fn auto_once(&self) -> bool {
        let status = self.status();
        let Some(config) = status.config else {
            return false;
        };
        if status.password_lost || !self.store().vault().is_unlocked() {
            return false;
        }
        let now = crate::time::now_ms();
        if !is_due(&chrono::Local, &config, status.last.map(|r| r.at), now) {
            // 下一次的时间可能过时了（过了一个时间点而那次已经做过）：顺便刷新
            self.publish(|_| {});
            return false;
        }
        let _guard = self.inner.lock.lock().await;
        // 等锁期间用户可能刚点了“立即备份”
        let status = self.status();
        let Some(config) = status.config else {
            return false;
        };
        if !is_due(&chrono::Local, &config, status.last.map(|r| r.at), now) {
            return false;
        }
        self.run_locked(false).await.is_ok()
    }

    /// 在核心的运行时上开始定时备份：启动约 2 分钟后检查一次（补做错过的），之后每分钟检查一次。
    /// 返回的任务一直运行，退出时 `abort` 即可。
    pub fn spawn_auto(&self) -> JoinHandle<()> {
        let this = self.clone();
        crate::runtime::spawn(async move {
            tokio::time::sleep(FIRST_CHECK).await;
            loop {
                this.auto_once().await;
                tokio::select! {
                    () = tokio::time::sleep(CHECK_EVERY) => {}
                    () = this.inner.wake.notified() => {}
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;
    use crate::backup::testkit::Machine;
    use crate::testkit::block_on;

    /// 北京时间（UTC+8）的 Unix 毫秒。
    fn at(tz: &FixedOffset, y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        tz.with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn beijing() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }

    fn config(frequency: Frequency, minute_of_day: u16, since: i64) -> ScheduleConfig {
        ScheduleConfig {
            folder: PathBuf::from("/backups"),
            frequency,
            minute_of_day,
            keep: 3,
            since,
        }
    }

    #[test]
    fn daily_slots() {
        let tz = beijing();
        // 2026-11-05 是星期四
        let now = at(&tz, 2026, 11, 5, 10, 0);
        let three = 3 * 60;
        assert_eq!(
            last_slot(&tz, Frequency::Daily, three, now),
            Some(at(&tz, 2026, 11, 5, 3, 0))
        );
        assert_eq!(
            next_slot(&tz, Frequency::Daily, three, now),
            Some(at(&tz, 2026, 11, 6, 3, 0))
        );
        // 刚好到点：算作这个时间点，下一次是明天
        let three_now = at(&tz, 2026, 11, 5, 3, 0);
        assert_eq!(
            last_slot(&tz, Frequency::Daily, three, three_now),
            Some(three_now)
        );
        assert_eq!(
            next_slot(&tz, Frequency::Daily, three, three_now),
            Some(at(&tz, 2026, 11, 6, 3, 0))
        );
        // 23:59
        assert_eq!(
            next_slot(&tz, Frequency::Daily, 23 * 60 + 59, now),
            Some(at(&tz, 2026, 11, 5, 23, 59))
        );
    }

    #[test]
    fn weekly_slots() {
        let tz = beijing();
        let now = at(&tz, 2026, 11, 5, 10, 0); // 星期四
        let monday = Frequency::Weekly { weekday: 1 };
        assert_eq!(
            last_slot(&tz, monday, 9 * 60, now),
            Some(at(&tz, 2026, 11, 2, 9, 0))
        );
        assert_eq!(
            next_slot(&tz, monday, 9 * 60, now),
            Some(at(&tz, 2026, 11, 9, 9, 0))
        );
        // 当天（星期四）还没到点：下一次是今天
        let thursday = Frequency::Weekly { weekday: 4 };
        assert_eq!(
            next_slot(&tz, thursday, 20 * 60, now),
            Some(at(&tz, 2026, 11, 5, 20, 0))
        );
        assert_eq!(
            last_slot(&tz, thursday, 20 * 60, now),
            Some(at(&tz, 2026, 10, 29, 20, 0))
        );
        let sunday = Frequency::Weekly { weekday: 7 };
        assert_eq!(
            next_slot(&tz, sunday, 0, now),
            Some(at(&tz, 2026, 11, 8, 0, 0))
        );
    }

    #[test]
    fn missed_backups_are_made_up_once() {
        let tz = beijing();
        let set_up = at(&tz, 2026, 11, 1, 12, 0);
        let c = config(Frequency::Daily, 3 * 60, set_up);
        // 设置之后还没到第一个时间点：不做
        assert!(!is_due(&tz, &c, None, at(&tz, 2026, 11, 1, 20, 0)));
        // 设置之前的时间点不补做（设置当天 03:00 早于设置时间）
        assert!(!is_due(&tz, &c, None, at(&tz, 2026, 11, 1, 12, 30)));
        // 第二天 03:00 到点
        assert!(is_due(&tz, &c, None, at(&tz, 2026, 11, 2, 3, 0)));
        // 做过了：同一个时间点不再做
        let done = at(&tz, 2026, 11, 2, 3, 0);
        assert!(!is_due(&tz, &c, Some(done), at(&tz, 2026, 11, 2, 15, 0)));
        // 电脑关了三天，启动后补做一次……
        let boot = at(&tz, 2026, 11, 5, 9, 0);
        assert!(is_due(&tz, &c, Some(done), boot));
        assert_eq!(next_run(&tz, &c, Some(done), boot), Some(boot));
        // ……补做后不再做，下一次是明天 03:00
        assert!(!is_due(&tz, &c, Some(boot), boot + 60_000));
        assert_eq!(
            next_run(&tz, &c, Some(boot), boot + 60_000),
            Some(at(&tz, 2026, 11, 6, 3, 0))
        );
        // 失败也算做过（等下一个时间点）；手动备份在时间点之后做过也算
        let manual = at(&tz, 2026, 11, 5, 8, 0);
        assert!(!is_due(&tz, &c, Some(manual), boot));
    }

    #[test]
    fn file_names_follow_the_rule() {
        let tz = beijing();
        let name = file_name(&tz, at(&tz, 2026, 11, 5, 3, 0));
        assert_eq!(name, "termlet-auto-20261105-030000.termlet-backup");
        assert_eq!(parse_file_name(&name), Some(20261105030000));
        for other in [
            "termlet-backup-2026-11-05-0300.termlet-backup",
            "termlet-auto-20261105-0300.termlet-backup",
            "termlet-auto-2026110x-030000.termlet-backup",
            "termlet-auto-20261105-030000.termlet-backup.bak",
            "my-termlet-auto-20261105-030000.termlet-backup",
            "termlet-auto-20261105-030000-1.termlet-backup",
        ] {
            assert_eq!(parse_file_name(other), None, "{other}");
        }
    }

    #[test]
    fn prune_only_deletes_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = |name: &str| dir.path().join(name);
        let own = [
            "termlet-auto-20261101-030000.termlet-backup",
            "termlet-auto-20261102-030000.termlet-backup",
            "termlet-auto-20261103-030000.termlet-backup",
            "termlet-auto-20261104-030000.termlet-backup",
            "termlet-auto-20261105-030000.termlet-backup",
        ];
        for name in own {
            std::fs::write(p(name), b"x").unwrap();
        }
        let others = [
            "termlet-backup-2026-10-01-1200.termlet-backup",
            "termlet-auto-notes.txt",
            "photo.jpg",
        ];
        for name in others {
            std::fs::write(p(name), b"x").unwrap();
        }
        // 名字符合规则的目录不删
        std::fs::create_dir(p("termlet-auto-20200101-000000.termlet-backup")).unwrap();
        assert_eq!(prune(dir.path(), 2).unwrap(), 3);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut expected: Vec<String> = others.iter().map(|s| s.to_string()).collect();
        expected.extend([
            "termlet-auto-20200101-000000.termlet-backup".to_string(),
            own[3].to_string(),
            own[4].to_string(),
        ]);
        expected.sort();
        assert_eq!(left, expected);
        // 份数没超过时不删
        assert_eq!(prune(dir.path(), 5).unwrap(), 0);
    }

    fn input(folder: &Path, password: Option<&str>) -> ScheduleInput {
        ScheduleInput {
            folder: folder.to_path_buf(),
            frequency: Frequency::Daily,
            minute_of_day: 3 * 60,
            keep: 2,
            password: password.map(SecretString::new),
        }
    }

    #[test]
    fn configure_checks_and_keeps_the_password_sealed() {
        let m = Machine::new();
        let schedule = BackupSchedule::new(m.backups.clone());
        let folder = m.path("auto");
        assert_eq!(
            block_on(schedule.configure(input(&folder, Some("long enough")))).unwrap_err(),
            BackupError::FolderMissing {
                path: folder.clone()
            }
        );
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(
            block_on(schedule.configure(input(&folder, Some("short")))).unwrap_err(),
            BackupError::PasswordTooShort { min: 8 }
        );
        assert_eq!(
            block_on(schedule.configure(input(&folder, None))).unwrap_err(),
            BackupError::PasswordTooShort { min: 8 },
            "第一次设置要有密码"
        );
        let mut bad = input(&folder, Some("long enough"));
        bad.keep = 0;
        assert_eq!(
            block_on(schedule.configure(bad)).unwrap_err(),
            BackupError::InvalidSchedule
        );
        assert_eq!(
            block_on(schedule.configure(input(Path::new("relative"), Some("long enough"))))
                .unwrap_err(),
            BackupError::NotAbsolute
        );
        block_on(schedule.configure(input(&folder, Some("long enough")))).unwrap();
        let status = schedule.status();
        assert!(status.config.is_some() && status.next.is_some());
        // 密码不以明文进数据库
        let blob = m.store.database().meta(META_SECRET).unwrap().unwrap();
        assert!(!blob.windows(11).any(|w| w == b"long enough"));
        // 更改设置时可以不重新输入密码
        let mut changed = input(&folder, None);
        changed.keep = 5;
        block_on(schedule.configure(changed)).unwrap();
        assert_eq!(schedule.status().config.unwrap().keep, 5);
        // 重新打开后设置还在
        let again = BackupSchedule::new(m.backups.clone());
        assert_eq!(again.status().config, schedule.status().config);
        block_on(again.disable()).unwrap();
        assert!(again.status().config.is_none());
        assert!(m.store.database().meta(META_SECRET).unwrap().is_none());
    }

    #[test]
    fn run_now_creates_backups_and_keeps_n() {
        let m = Machine::new();
        let h = m.add_host("web", "10.0.0.1", "pw");
        let schedule = BackupSchedule::new(m.backups.clone());
        let folder = m.path("auto");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(
            folder.join("termlet-backup-manual.termlet-backup"),
            b"manual",
        )
        .unwrap();
        // 旧的定时备份（假的内容）：超出份数时被删
        for day in 1..=3 {
            std::fs::write(
                folder.join(format!("termlet-auto-2020010{day}-030000.termlet-backup")),
                b"old",
            )
            .unwrap();
        }
        block_on(schedule.configure(input(&folder, Some("long enough")))).unwrap();
        let record = block_on(schedule.run_now()).unwrap();
        assert!(record.ok() && record.manual, "{record:?}");
        assert_eq!(record.deleted, 2, "保留 2 份：这次的与最新的旧备份");
        let file = folder.join(record.file.as_ref().unwrap());
        // 用设置的密码能恢复
        let summary = block_on(
            m.backups
                .inspect_backup(file.clone(), Some(SecretString::new("long enough"))),
        )
        .unwrap();
        assert_eq!(summary.counts.hosts, 1);
        assert!(folder.join("termlet-backup-manual.termlet-backup").exists());
        assert!(
            folder
                .join("termlet-auto-20200103-030000.termlet-backup")
                .exists()
        );
        assert_eq!(schedule.status().last, Some(record.clone()));
        // 上次的结果重启后还在
        assert_eq!(
            BackupSchedule::new(m.backups.clone()).status().last,
            Some(record)
        );
        let _ = h;

        // 文件夹不见了：记下失败的原因
        std::fs::remove_dir_all(&folder).unwrap();
        let record = block_on(schedule.run_now()).unwrap();
        assert_eq!(
            record.error,
            Some(BackupError::FolderMissing {
                path: folder.clone()
            })
        );
        // 失败的原因可以序列化（存在本机，重启后显示）
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(serde_json::from_str::<RunRecord>(&json).unwrap(), record);
    }

    #[test]
    fn auto_makes_up_a_missed_backup_once() {
        let m = Machine::new();
        let schedule = BackupSchedule::new(m.backups.clone());
        let folder = m.path("auto");
        std::fs::create_dir(&folder).unwrap();
        block_on(schedule.configure(input(&folder, Some("long enough")))).unwrap();
        // 刚设置：最近的时间点在设置之前，不做
        assert!(!block_on(schedule.auto_once()));
        // 把设置的时间改到两天前：错过了，补做一次
        let mut config = schedule.status().config.unwrap();
        config.since -= 2 * 24 * 3600 * 1000;
        m.store
            .database()
            .set_meta(META_CONFIG, &serde_json::to_vec(&config).unwrap())
            .unwrap();
        let schedule = BackupSchedule::new(m.backups.clone());
        assert!(
            schedule.status().next.unwrap() <= crate::time::now_ms(),
            "等着补做：下一次是现在"
        );
        assert!(block_on(schedule.auto_once()));
        assert!(!block_on(schedule.auto_once()), "只补做一次");
        let record = schedule.status().last.unwrap();
        assert!(record.ok() && !record.manual);
    }
}
