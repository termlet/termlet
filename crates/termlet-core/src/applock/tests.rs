//! 应用锁的测试：锁定与密码、系统验证可用性的缓存、自动锁定。

use std::num::NonZeroIsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::*;
use crate::testkit::{block_on, memory_store, wait_until};

/// 假的系统功能：空闲时间、锁屏状态、系统验证都由测试设定，并记下调用。
struct FakePlatform {
    idle: Mutex<Option<Duration>>,
    session: Mutex<Option<bool>>,
    /// 依次给出的可用性检查结果（用完后一直是最后一个）
    available: Mutex<Vec<bool>>,
    check_delay: Duration,
    checks: AtomicUsize,
    verdict: Mutex<Verification>,
    asked: Mutex<Vec<(Option<NonZeroIsize>, String)>>,
}

impl FakePlatform {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(None),
            session: Mutex::new(None),
            available: Mutex::new(vec![true]),
            check_delay: Duration::ZERO,
            checks: AtomicUsize::new(0),
            verdict: Mutex::new(Verification::Verified),
            asked: Mutex::new(Vec::new()),
        })
    }

    fn with_availability(answers: &[bool], delay: Duration) -> Arc<Self> {
        let mut fake = Arc::try_unwrap(Self::new()).ok().unwrap();
        fake.available = Mutex::new(answers.to_vec());
        fake.check_delay = delay;
        Arc::new(fake)
    }
}

impl LockPlatform for FakePlatform {
    fn idle_time(&self) -> Option<Duration> {
        *self.idle.lock().unwrap()
    }

    fn session_locked(&self) -> Option<bool> {
        *self.session.lock().unwrap()
    }

    fn system_auth_available(&self) -> bool {
        self.checks.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.check_delay);
        let mut answers = self.available.lock().unwrap();
        if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0]
        }
    }

    fn system_verify(&self, window: Option<NonZeroIsize>, message: &str) -> Verification {
        self.asked
            .lock()
            .unwrap()
            .push((window, message.to_string()));
        *self.verdict.lock().unwrap()
    }
}

fn open(store: &Arc<Store>, platform: &Arc<FakePlatform>) -> AppLock {
    AppLock::open_with(store.clone(), platform.clone()).unwrap()
}

fn pw(s: &str) -> SecretString {
    SecretString::new(s)
}

/// 启用、锁定、输错、解锁、改密码、重新打开时按设置锁定、系统验证、停用。
#[test]
fn lifecycle() {
    let store = memory_store();
    let platform = FakePlatform::new();
    let lock = open(&store, &platform);
    // 没有设置密码：不会锁定
    assert_eq!(lock.status(), LockStatus::default());
    assert!(matches!(lock.lock_now(), Err(LockError::NotEnabled)));
    assert!(matches!(
        block_on(lock.set_password(None, &pw("123"))),
        Err(LockError::TooShort)
    ));

    let mut events = lock.subscribe();
    block_on(lock.set_password(None, &pw("secret-1"))).unwrap();
    assert!(events.has_changed().unwrap());
    assert_eq!(
        *events.borrow_and_update(),
        LockStatus {
            enabled: true,
            locked: false
        }
    );

    lock.lock_now().unwrap();
    assert!(lock.is_locked());
    assert_eq!(
        *events.borrow_and_update(),
        LockStatus {
            enabled: true,
            locked: true
        }
    );
    // 已经锁定时再锁不再通知
    lock.lock_now().unwrap();
    assert!(!events.has_changed().unwrap());

    assert!(matches!(
        block_on(lock.unlock(&pw("wrong"))),
        Err(LockError::WrongPassword)
    ));
    assert!(lock.is_locked(), "密码错误应保持锁定");
    block_on(lock.unlock(&pw("secret-1"))).unwrap();
    assert!(!lock.is_locked());
    assert_eq!(
        *events.borrow_and_update(),
        LockStatus {
            enabled: true,
            locked: false
        }
    );

    // 修改密码要当前密码
    assert!(matches!(
        block_on(lock.set_password(Some(&pw("wrong")), &pw("secret-2"))),
        Err(LockError::WrongPassword)
    ));
    assert!(matches!(
        block_on(lock.set_password(None, &pw("secret-2"))),
        Err(LockError::WrongPassword)
    ));
    block_on(lock.set_password(Some(&pw("secret-1")), &pw("secret-2"))).unwrap();

    // 重新打开：默认“启动时锁定”
    let reopened = open(&store, &platform);
    assert_eq!(
        reopened.status(),
        LockStatus {
            enabled: true,
            locked: true
        }
    );
    block_on(reopened.unlock_with_system(None, "解锁 Termlet")).unwrap();
    assert!(!reopened.is_locked(), "系统验证后应解锁");
    store
        .update_local_settings(|s| s.app_lock.on_startup = false)
        .unwrap();
    assert_eq!(
        open(&store, &platform).status(),
        LockStatus {
            enabled: true,
            locked: false
        },
        "没有设置启动时锁定时不应锁定"
    );

    // 停用要密码，停用后再打开不会锁定
    assert!(matches!(
        block_on(lock.disable(&pw("secret-1"))),
        Err(LockError::WrongPassword)
    ));
    lock.lock_now().unwrap();
    block_on(lock.disable(&pw("secret-2"))).unwrap();
    assert_eq!(lock.status(), LockStatus::default(), "停用时一并解锁");
    assert!(matches!(
        block_on(lock.disable(&pw("secret-2"))),
        Err(LockError::NotEnabled)
    ));
    store
        .update_local_settings(|s| s.app_lock.on_startup = true)
        .unwrap();
    assert_eq!(open(&store, &platform).status(), LockStatus::default());
    assert!(store.database().meta(META_PASSWORD).unwrap().is_none());
}

/// 连续输错的次数累计（修改、停用时验证当前密码也算），解锁成功后清零；等待随次数变长、有上限。
#[test]
fn failures_accumulate_and_reset() {
    let store = memory_store();
    let lock = open(&store, &FakePlatform::new());
    block_on(lock.set_password(None, &pw("secret-1"))).unwrap();
    lock.lock_now().unwrap();
    for _ in 0..2 {
        assert!(block_on(lock.unlock(&pw("nope"))).is_err());
    }
    assert!(block_on(lock.disable(&pw("nope"))).is_err());
    assert_eq!(lock.inner.failures.load(Ordering::Relaxed), 3);
    block_on(lock.unlock(&pw("secret-1"))).unwrap();
    assert_eq!(lock.inner.failures.load(Ordering::Relaxed), 0);

    // 每次多等一个单位（测试中是 1 毫秒），最多 5 个
    lock.inner.failures.store(10, Ordering::Relaxed);
    let began = std::time::Instant::now();
    assert!(block_on(lock.unlock(&pw("nope"))).is_err());
    assert!(began.elapsed() >= lock.inner.delay_unit * MAX_DELAY_STEPS);
}

/// 存在 meta 中的是带参数与盐的哈希，没有明文；每次设置的盐不同。
#[test]
fn stored_hash_has_no_plaintext() {
    let store = memory_store();
    let lock = open(&store, &FakePlatform::new());
    block_on(lock.set_password(None, &pw("correct horse"))).unwrap();
    let first = store.database().meta(META_PASSWORD).unwrap().unwrap();
    let text = String::from_utf8(first.clone()).unwrap();
    assert!(!text.contains("correct horse"));
    let hash: PasswordHash = serde_json::from_slice(&first).unwrap();
    assert!(hash.is_sane());
    assert_eq!(hash.v, 1);
    assert_eq!(
        data_encoding::BASE64
            .decode(hash.salt.as_bytes())
            .unwrap()
            .len(),
        16
    );
    assert!(hash.verify(&pw("correct horse")).unwrap());
    assert!(!hash.verify(&pw("correct horse ")).unwrap());

    block_on(lock.set_password(Some(&pw("correct horse")), &pw("correct horse"))).unwrap();
    let second = store.database().meta(META_PASSWORD).unwrap().unwrap();
    assert_ne!(first, second, "盐每次重新生成");
    // 状态与日志中都没有密码
    assert!(!format!("{lock:?}").contains("correct"));
}

/// 发布构建用的参数（64 MB、3 次、4 路，与主密码相同）在认可的范围内。
#[test]
fn release_parameters_match_previous_version() {
    assert_eq!(ARGON2_PARAMS, (65536, 3, 4));
    let (memory_kib, iterations, parallelism) = ARGON2_PARAMS;
    let hash = PasswordHash {
        v: 1,
        memory_kib,
        iterations,
        parallelism,
        salt: data_encoding::BASE64.encode(&[1u8; 16]),
        hash: data_encoding::BASE64.encode(&[2u8; HASH_LEN]),
    };
    assert!(hash.is_sane());
}

/// 哈希损坏、参数离谱时当作没有启用，不会把用户锁在外面。
#[test]
fn corrupted_hash_is_treated_as_disabled() {
    let store = memory_store();
    for raw in [
        &b"not json"[..],
        br#"{"v":1,"memoryKib":99999999,"iterations":3,"parallelism":4,"salt":"AAAAAAAAAAA=","hash":"AAAA"}"#,
        b"",
    ] {
        store.database().set_meta(META_PASSWORD, raw).unwrap();
        assert_eq!(
            open(&store, &FakePlatform::new()).status(),
            LockStatus::default()
        );
    }
}

/// 长度按字符算：六个汉字可以，两个不行。
#[test]
fn password_length_counts_characters() {
    let store = memory_store();
    let lock = open(&store, &FakePlatform::new());
    assert!(matches!(
        block_on(lock.set_password(None, &pw("密码"))),
        Err(LockError::TooShort)
    ));
    block_on(lock.set_password(None, &pw("锁定密码六字"))).unwrap();
    lock.lock_now().unwrap();
    block_on(lock.unlock(&pw("锁定密码六字"))).unwrap();
}

#[test]
fn system_unlock_outcomes() {
    let store = memory_store();
    let platform = FakePlatform::new();
    let lock = open(&store, &platform);
    assert!(matches!(
        block_on(lock.unlock_with_system(None, "x")),
        Err(LockError::NotEnabled)
    ));
    block_on(lock.set_password(None, &pw("secret-1"))).unwrap();
    lock.lock_now().unwrap();

    *platform.verdict.lock().unwrap() = Verification::Canceled;
    let hwnd = NonZeroIsize::new(0x1234);
    let long = "解".repeat(300);
    assert!(matches!(
        block_on(lock.unlock_with_system(hwnd, &long)),
        Err(LockError::System(Verification::Canceled))
    ));
    assert!(lock.is_locked());
    {
        let asked = platform.asked.lock().unwrap();
        assert_eq!(asked[0].0, hwnd, "验证框属于传入的窗口");
        assert_eq!(
            asked[0].1.chars().count(),
            MAX_MESSAGE_CHARS,
            "提示文字截断"
        );
    }

    *platform.verdict.lock().unwrap() = Verification::Verified;
    block_on(lock.unlock_with_system(hwnd, "解锁 Termlet")).unwrap();
    assert!(!lock.is_locked());

    // 不可用：不弹出验证
    let unavailable = FakePlatform::with_availability(&[false], Duration::ZERO);
    let lock = open(&store, &unavailable);
    assert!(lock.is_locked());
    assert!(matches!(
        block_on(lock.unlock_with_system(None, "x")),
        Err(LockError::System(Verification::Unavailable))
    ));
    assert!(unavailable.asked.lock().unwrap().is_empty());
}

/// 不可用的结果过一会儿重新检查（首次检查失败、之后才设置 Windows Hello）；可用的结果一直有效。
#[test]
fn availability_is_cached() {
    let store = memory_store();
    let platform = FakePlatform::with_availability(&[false, true], Duration::ZERO);
    let lock = open(&store, &platform);
    assert!(!block_on(lock.system_available()));
    // 短时间内再问不重复检查
    assert!(!block_on(lock.system_available()));
    assert_eq!(platform.checks.load(Ordering::SeqCst), 1);
    std::thread::sleep(lock.inner.unavailable_ttl + Duration::from_millis(20));
    assert!(block_on(lock.system_available()));
    assert_eq!(platform.checks.load(Ordering::SeqCst), 2);
    std::thread::sleep(lock.inner.unavailable_ttl + Duration::from_millis(20));
    assert!(block_on(lock.system_available()));
    assert_eq!(platform.checks.load(Ordering::SeqCst), 2);
}

/// 并发的检查共用一次。
#[test]
fn concurrent_availability_checks_share_one() {
    let store = memory_store();
    let platform = FakePlatform::with_availability(&[true], Duration::from_millis(50));
    let lock = open(&store, &platform);
    let (a, b) =
        block_on(async { futures::join!(lock.system_available(), lock.system_available()) });
    assert!(a && b);
    assert_eq!(platform.checks.load(Ordering::SeqCst), 1);
}

#[test]
fn auto_lock_rules() {
    let settings = |idle_minutes: u32, on_system_lock: bool| AppLockSettings {
        idle_minutes,
        on_system_lock,
        ..Default::default()
    };
    let min = |m: u64| Some(Duration::from_secs(m * 60));
    // 空闲
    assert_eq!(
        auto_lock_reason(&settings(15, false), min(15), None, None),
        Some(LockReason::Idle)
    );
    assert_eq!(
        auto_lock_reason(&settings(15, false), min(14), None, None),
        None
    );
    assert_eq!(
        auto_lock_reason(&settings(0, false), min(10_000), None, None),
        None,
        "0 表示从不"
    );
    assert_eq!(
        auto_lock_reason(&settings(15, false), None, None, None),
        None,
        "不支持时不锁定"
    );
    // 超过上限的按上限
    assert_eq!(
        auto_lock_reason(&settings(u32::MAX, false), min(24 * 60), None, None),
        Some(LockReason::Idle)
    );
    // 锁屏：只在从未锁屏变为锁屏时
    let s = settings(0, true);
    assert_eq!(
        auto_lock_reason(&s, None, Some(false), Some(true)),
        Some(LockReason::SystemLock)
    );
    assert_eq!(auto_lock_reason(&s, None, Some(true), Some(true)), None);
    assert_eq!(
        auto_lock_reason(&s, None, None, Some(true)),
        None,
        "第一次看到的状态只作基准"
    );
    assert_eq!(auto_lock_reason(&s, None, Some(false), None), None);
    assert_eq!(
        auto_lock_reason(&settings(0, false), None, Some(false), Some(true)),
        None,
        "没有开启系统锁屏时锁定"
    );
}

/// 两次检查之间睡眠过（墙上时间跳过很久）。
#[test]
fn sleep_lock_rules() {
    let settings = |idle_minutes: u32, on_system_lock: bool| AppLockSettings {
        idle_minutes,
        on_system_lock,
        ..Default::default()
    };
    let secs = Duration::from_secs;
    // 正常的检查间隔
    assert_eq!(sleep_lock_reason(&settings(5, true), secs(3)), None);
    // 系统锁屏时锁定：睡眠过就锁
    assert_eq!(
        sleep_lock_reason(&settings(0, true), SLEEP_GAP),
        Some(LockReason::SystemLock)
    );
    // 只有空闲锁定：睡眠的时间不短于设置的分钟数才锁
    assert_eq!(sleep_lock_reason(&settings(5, false), secs(4 * 60)), None);
    assert_eq!(
        sleep_lock_reason(&settings(5, false), secs(5 * 60)),
        Some(LockReason::Idle)
    );
    assert_eq!(
        sleep_lock_reason(&settings(0, false), secs(24 * 3600)),
        None
    );
}

/// 真实系统的查询不出错（Windows 上空闲时间一定读得到）。
#[test]
fn system_platform_queries() {
    let idle = SystemPlatform.idle_time();
    if cfg!(windows) {
        assert!(idle.is_some());
    }
    let _ = SystemPlatform.session_locked();
}

/// 手动检查真实的系统状态：`cargo test -p termlet-core applock::tests::system_state_manual -- --ignored --nocapture`
#[test]
#[ignore = "读取真实的 Windows Hello 状态，手动运行"]
fn system_state_manual() {
    let platform = SystemPlatform;
    println!(
        "空闲 {:?}，锁屏 {:?}，Windows Hello 可用 {}",
        platform.idle_time(),
        platform.session_locked(),
        platform.system_auth_available()
    );
}

/// 手动检查真实的 Windows Hello 验证（会弹出验证框，等人操作）：
/// `cargo test -p termlet-core applock::tests::windows_hello_verify_manual -- --ignored --nocapture`
#[test]
#[ignore = "会弹出 Windows Hello 验证框，手动运行"]
fn windows_hello_verify_manual() {
    println!(
        "验证结果 {:?}",
        SystemPlatform.system_verify(None, "Termlet 测试")
    );
}

/// 自动锁定：空闲达到设置的分钟数时锁定；系统锁屏时锁定；没有启用时不动。
#[test]
fn auto_lock_by_idle_and_session_lock() {
    let store = memory_store();
    store
        .update_local_settings(|s| {
            s.app_lock.idle_minutes = 5;
            s.app_lock.on_system_lock = true;
            s.app_lock.on_startup = false;
        })
        .unwrap();
    let platform = FakePlatform::new();
    let lock = open(&store, &platform);
    let task = lock.spawn_auto_lock();

    // 没有启用：空闲再久也不锁
    *platform.idle.lock().unwrap() = Some(Duration::from_secs(3600));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(lock.status(), LockStatus::default());

    block_on(lock.set_password(None, &pw("secret-1"))).unwrap();
    wait_until("空闲时锁定", || lock.is_locked());
    *platform.idle.lock().unwrap() = Some(Duration::from_secs(1));
    block_on(lock.unlock(&pw("secret-1"))).unwrap();

    *platform.session.lock().unwrap() = Some(false);
    std::thread::sleep(Duration::from_millis(50));
    assert!(!lock.is_locked());
    *platform.session.lock().unwrap() = Some(true);
    wait_until("锁屏时锁定", || lock.is_locked());

    // 锁屏状态一直不变时，解锁后不会马上又锁上
    block_on(lock.unlock(&pw("secret-1"))).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(!lock.is_locked());

    // 所有 AppLock 丢弃后检查自行结束
    drop(lock);
    block_on(task).unwrap();
}

/// 固定向量：由 argon2 0.5 写出的锁定密码哈希（正式参数，盐写死）。升级加密库后必须照常校验。
const FIXED_HASH: &str = r#"{"v":1,"memoryKib":65536,"iterations":3,"parallelism":4,"salt":"dGVybWxldC1maXhlZC0wMQ==","hash":"T9315tAbIem+bgqiI5ApksRzsHaWM+vHE+HHLHcxtEc="}"#;

#[test]
fn fixed_password_hash() {
    let hash: PasswordHash = serde_json::from_str(FIXED_HASH).unwrap();
    assert!(hash.is_sane());
    assert_eq!(
        (hash.memory_kib, hash.iterations, hash.parallelism),
        ARGON2_PARAMS
    );
    assert!(hash.verify(&pw("correct horse battery staple")).unwrap());
    assert!(!hash.verify(&pw("wrong password")).unwrap());

    let store = memory_store();
    store
        .database()
        .set_meta(META_PASSWORD, FIXED_HASH.as_bytes())
        .unwrap();
    let lock = open(&store, &FakePlatform::new());
    lock.lock_now().unwrap();
    assert!(matches!(
        block_on(lock.unlock(&pw("wrong password"))),
        Err(LockError::WrongPassword)
    ));
    block_on(lock.unlock(&pw("correct horse battery staple"))).unwrap();
    assert!(!lock.is_locked());
}
