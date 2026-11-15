//! 部署公钥（相当于 ssh-copy-id）：用主机保存的凭据登录，
//! 把公钥追加到 `~/.ssh/authorized_keys`（已存在时不重复添加），再只用这把私钥单独登录一次验证；
//! 可选验证通过后把主机的认证改为这把密钥。

use std::sync::Arc;
use std::time::Duration;

use super::{KeyVaultError, blocking, private_key};
use crate::id::{GroupId, HostId, KeyId};
use crate::model::{Auth, Host, KeyEntry, KeySource, Profile, SecretField};
use crate::ssh::plan::{AuthPlan, KeyMaterial};
use crate::ssh::prompt::PromptFuture;
use crate::ssh::{
    Answer, Prompt, Prompter, SshError, TerminalSpec, connect, exec_with_input, remembering,
};
use crate::sshkey::{DeployOutcome, deploy_outcome, deploy_script};
use crate::store::{Owner, Store};

/// 部署一台主机（两次连接 + 一次远程脚本）的总时限，不含等用户回答询问（指纹、密码）的时间。
pub const DEPLOY_TIMEOUT: Duration = Duration::from_secs(90);
/// 远程脚本的输出（标准输出、标准错误各自）只保留这么多字节。
pub const DEPLOY_OUTPUT_LIMIT: usize = 16 * 1024;

/// 部署的请求。
pub struct DeployRequest {
    pub key: KeyId,
    pub host: HostId,
    /// 验证登录成功后把主机的认证方式改为这把密钥（原来的密码、私钥一并删除）
    pub use_key: bool,
    /// 用主机凭据登录时的询问：确认指纹、输入密码与验证码。不询问时传 [`crate::ssh::no_prompt`]（未确认指纹的
    /// 主机要先在终端中连接一次），此时未确认指纹报 [`SshError::HostKeyRejected`]、需要输入时报
    /// [`SshError::AuthCancelled`]
    pub prompter: Prompter,
}

/// 部署成功的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployReport {
    /// 新增了公钥，或 authorized_keys 中已有这把公钥（没有重复添加）
    pub outcome: DeployOutcome,
    /// 只用这把私钥验证登录的结果：`None` 为通过；失败时是原因（例如服务器不接受公钥认证、要求再输入验证码）
    pub verify_error: Option<SshError>,
    /// 已把主机的认证方式改为这把密钥（请求了 `use_key` 且验证通过）
    pub switched: bool,
}

impl DeployReport {
    pub fn verified(&self) -> bool {
        self.verify_error.is_none()
    }
}

/// 把密钥库中的公钥部署到主机。第一次连接失败（指纹未确认、认证失败等）、脚本失败返回错误；
/// 验证登录失败不算错误，结果中 `verify_error` 为原因。除去等用户回答的时间，全程不超过 [`DEPLOY_TIMEOUT`]
/// （询问期间不会超时，免得用户还在输入密码，部署已经报超时）。
///
/// 作者：ruan
pub async fn deploy(
    store: Arc<Store>,
    request: DeployRequest,
) -> Result<DeployReport, KeyVaultError> {
    let clock = PromptClock::default();
    let request = DeployRequest {
        prompter: clock.wrap(request.prompter),
        ..request
    };
    let start = tokio::time::Instant::now();
    let run = run(store, request);
    tokio::pin!(run);
    loop {
        let deadline = start + DEPLOY_TIMEOUT + clock.waited();
        tokio::select! {
            result = &mut run => return result,
            _ = tokio::time::sleep_until(deadline) => {
                if !clock.asking() && start + DEPLOY_TIMEOUT + clock.waited() <= tokio::time::Instant::now() {
                    return Err(KeyVaultError::Timeout);
                }
            }
        }
    }
}

/// 记下等用户回答询问用了多久（不算进部署的时限）。
#[derive(Clone, Default)]
struct PromptClock {
    /// 已经结束的询问共用了多久；正在询问时是从何时开始的
    state: Arc<std::sync::Mutex<(Duration, Option<tokio::time::Instant>)>>,
}

impl PromptClock {
    fn wrap(&self, inner: Prompter) -> Prompter {
        let clock = self.clone();
        Arc::new(move |prompt: Prompt| -> PromptFuture {
            let inner = inner.clone();
            let clock = clock.clone();
            Box::pin(async move {
                clock.lock().1 = Some(tokio::time::Instant::now());
                let answer = inner(prompt).await;
                let mut state = clock.lock();
                if let Some(since) = state.1.take() {
                    state.0 += since.elapsed();
                }
                answer
            })
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Duration, Option<tokio::time::Instant>)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn asking(&self) -> bool {
        self.lock().1.is_some()
    }

    /// 询问共用了多久（含正在进行的）。
    fn waited(&self) -> Duration {
        let state = self.lock();
        state.0 + state.1.map_or(Duration::ZERO, |since| since.elapsed())
    }
}

async fn run(store: Arc<Store>, request: DeployRequest) -> Result<DeployReport, KeyVaultError> {
    let DeployRequest {
        key,
        host,
        use_key,
        prompter,
    } = request;
    // 读条目与私钥（解密）在阻塞线程上
    let (entry, spec, private) = {
        let store = store.clone();
        let (key, host) = (key.clone(), host.clone());
        blocking(move || {
            let entry = store
                .get::<KeyEntry>(&key)
                .ok_or(KeyVaultError::NotFound)?
                .doc;
            let record = store
                .get::<Host>(&host)
                .ok_or(KeyVaultError::HostNotFound)?;
            if !record.doc.is_ssh() {
                return Err(KeyVaultError::NotSsh);
            }
            let private = private_key(&store, &key)?.to_openssh()?;
            let spec = TerminalSpec::for_host(&store, &host).map_err(KeyVaultError::Connect)?;
            Ok((entry, spec, private))
        })
        .await?
    };
    let script = deploy_script(&entry.public_key)?;
    let options = crate::ssh::ConnectOptions {
        keepalive: None,
        ..spec.connect.clone()
    };
    // 两次连接共用记住的密码（跳板机的密码只问一次）
    let prompter = remembering(prompter);

    // 1. 用主机的凭据登录，以 sh 执行脚本（脚本经标准输入传入，不受登录 shell 种类影响）
    let connection = connect(store.clone(), &spec.plan, prompter.clone(), &options)
        .await
        .map_err(KeyVaultError::Connect)?;
    let remaining = DEPLOY_TIMEOUT.saturating_sub(Duration::from_secs(5));
    let result = exec_with_input(
        &connection,
        "sh",
        Some(script.as_bytes()),
        DEPLOY_OUTPUT_LIMIT,
        remaining,
    )
    .await;
    connection.disconnect().await;
    let result = result.map_err(|error| KeyVaultError::DeployFailed {
        exit_code: None,
        detail: error.to_string(),
    })?;
    let stdout = String::from_utf8_lossy(&result.stdout);
    let outcome = match (result.exit_code, deploy_outcome(&stdout)) {
        (Some(0), Some(outcome)) => outcome,
        (exit_code, _) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            return Err(KeyVaultError::DeployFailed {
                exit_code,
                detail: detail.to_string(),
            });
        }
    };
    tracing::info!(host = %host, key = %key, ?outcome, "公钥已部署");

    // 2. 只用这把私钥验证（经同样的跳板机）：不用密码、agent，目标主机上也不回答键盘交互
    let mut verify = spec.plan.clone();
    if let Some(target) = verify.hops.last_mut() {
        target.auth = AuthPlan::Key {
            key: KeyMaterial::Text(private),
            passphrase: None,
            label: entry.name.clone(),
            // 验证的是部署的公钥本身，不出示附带的证书
            certificate: None,
        };
        target.totp = None;
    }
    let target_label = spec.plan.target().label.clone();
    let quiet: Prompter = {
        let prompter = prompter.clone();
        Arc::new(move |prompt: Prompt| -> PromptFuture {
            let on_target = match &prompt {
                Prompt::Password { host, .. } | Prompt::KeyboardInteractive { host, .. } => {
                    *host == target_label
                }
                // 验证用的私钥没有加密，问私钥密码的只会是跳板机；代理命令在第一次连接时已经确认过
                Prompt::Passphrase { .. } | Prompt::HostKey(_) | Prompt::ProxyCommand(_) => false,
            };
            if on_target {
                Box::pin(async { Answer::Cancel })
            } else {
                prompter(prompt)
            }
        })
    };
    let verify_error = match connect(store.clone(), &verify, quiet, &options).await {
        Ok(connection) => {
            connection.disconnect().await;
            None
        }
        Err(error) => {
            tracing::info!(host = %host, key = %key, "只用密钥登录的验证没有通过：{error}");
            Some(error)
        }
    };

    // 3. 改用这把密钥
    let switched = use_key && verify_error.is_none();
    if switched {
        use_library_key(store, host, key).await?;
    }
    Ok(DeployReport {
        outcome,
        verify_error,
        switched,
    })
}

/// 把主机的认证方式改为密钥库中的密钥：只改认证方式，其他设置与命令行工具专用凭据不变；
/// 主机原来的密码、粘贴的私钥与私钥密码删除。主机没填用户名、原来用的是凭据档案的用户名时，
/// 把它写进主机（改用密钥后不再取档案的用户名，登录的账号不变）。
pub async fn use_library_key(
    store: Arc<Store>,
    host: HostId,
    key: KeyId,
) -> Result<(), KeyVaultError> {
    blocking(move || {
        if store.get::<KeyEntry>(&key).is_none() {
            return Err(KeyVaultError::NotFound);
        }
        let record = store
            .get::<Host>(&host)
            .ok_or(KeyVaultError::HostNotFound)?;
        let mut doc = record.doc;
        if doc.username.trim().is_empty() {
            let chain = store.group_chain(record.parent.map(GroupId::from_raw).as_ref());
            let profile = match &doc.auth {
                Auth::Profile { profile } => Some(profile.clone()),
                Auth::Inherit => chain.iter().find_map(|g| g.doc.defaults.profile.clone()),
                _ => None,
            };
            if let Some(username) = profile
                .and_then(|id| store.get::<Profile>(&id))
                .map(|p| p.doc.username)
                .filter(|u| !u.trim().is_empty())
            {
                doc.username = username;
            }
        }
        doc.auth = Auth::Key {
            source: KeySource::Library { key },
        };
        store.update(&host, doc)?;
        let owner = Owner::host(&host);
        for field in [
            SecretField::Password,
            SecretField::PrivateKey,
            SecretField::Passphrase,
        ] {
            if store.has_secret(&owner, field) {
                store.set_secret(&owner, field, None)?;
            }
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prompt_clock_counts_time_spent_waiting_for_answers() {
        let clock = PromptClock::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let rx = Arc::new(std::sync::Mutex::new(Some(rx)));
        let inner: Prompter = Arc::new(move |_| -> PromptFuture {
            let rx = rx.lock().unwrap().take();
            Box::pin(async move {
                if let Some(rx) = rx {
                    let _ = rx.await;
                }
                Answer::Cancel
            })
        });
        let prompter = clock.wrap(inner);
        let asked = tokio::spawn(prompter(Prompt::Passphrase {
            key: String::new(),
            retry: false,
            id: String::new(),
        }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(clock.asking());
        tx.send(()).unwrap();
        asked.await.unwrap();
        assert!(!clock.asking());
        assert!(clock.waited() >= Duration::from_millis(30));
    }
}
