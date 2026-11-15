//! 认证：密码、私钥（可带 OpenSSH 证书）、ssh-agent / Pageant（含其中的证书）、键盘交互（验证码由保存的 TOTP 密钥自动回答）。
//!
//! - 密码没有保存、或保存的不对时询问（最多三次）；服务器只开了键盘交互（PAM 常见）时用键盘交互回答密码。
//! - 私钥被加密而没有保存私钥密码时询问。
//! - 私钥有证书（指定的证书文件、私钥文件旁同名的 `-cert.pub`、密钥库中附带的）时先出示证书；
//!   服务器不接受而还接受公钥时再用私钥本身（`authorized_keys` 中有这把公钥的服务器照常登录）。
//!   证书与私钥对不上、读不了、是主机证书时不出示，记下原因。
//! - agent 中的密钥与证书按 agent 列出的顺序逐个尝试（与 OpenSSH 相同）。
//! - 最终没能登录而出示过证书（或证书没能用上）时，说明证书的问题（过期、未生效、用户名不在允许的列表中、
//!   服务器不信任 CA，见 [`crate::sshcert::CertProblem`]），而不只是“认证失败”。
//! - 一种方式通过后服务器还要求别的（`partial_success`，例如私钥加验证码），继续用键盘交互。
//! - 快速连接先试 agent 中的密钥，再询问密码。

use std::path::Path;
use std::sync::Arc;

use russh::MethodKind;
use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{Certificate, HashAlg, PrivateKey, PrivateKeyWithHashAlg};

use crate::secret::SecretString;
use crate::ssh::SshError;
use crate::ssh::handler::ClientHandler;
use crate::ssh::plan::{AuthPlan, CertMaterial, Hop, KeyMaterial};
use crate::ssh::prompt::{Answer, Prompt, Prompter};
use crate::sshcert::{self, CertProblem, CertReadIssue};
use crate::sshkey::{self, KeyError};
use crate::totp::Totp;

/// 询问密码、私钥密码的次数。
const MAX_ATTEMPTS: usize = 3;
/// 键盘交互的最多轮数（服务器可以连续发问）。
const MAX_ROUNDS: usize = 10;

/// 认证的中间结果。
enum Step {
    Success,
    /// 失败；服务器建议改用的方式
    Failed(Vec<MethodKind>),
    /// 这一种通过了，还需要别的方式
    Partial(Vec<MethodKind>),
}

impl From<AuthResult> for Step {
    fn from(result: AuthResult) -> Self {
        match result {
            AuthResult::Success => Step::Success,
            AuthResult::Failure {
                remaining_methods,
                partial_success: true,
            } => Step::Partial(remaining_methods.to_vec()),
            AuthResult::Failure {
                remaining_methods, ..
            } => Step::Failed(remaining_methods.to_vec()),
        }
    }
}

impl Step {
    /// 日志中的写法：`success`、`failed(password, publickey)`、`partial(keyboard-interactive)`。
    fn describe(&self) -> String {
        match self {
            Step::Success => "success".into(),
            Step::Failed(methods) => format!("failed({})", method_names(methods)),
            Step::Partial(methods) => format!("partial({})", method_names(methods)),
        }
    }
}

fn method_names(methods: &[MethodKind]) -> String {
    methods
        .iter()
        .map(<&'static str>::from)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) struct Authenticator<'a> {
    handle: &'a mut Handle<ClientHandler>,
    hop: &'a Hop,
    prompter: &'a Prompter,
    agent_path: Option<&'a str>,
    /// 已用过保存的 TOTP 自动回答（只用一次，避免错误时反复提交）
    totp_used: bool,
    /// 证书没能用来登录的原因（第一次出现的）：最终认证失败时据此说明
    cert_problem: Option<CertProblem>,
    /// 服务器接受了的证书（登录后提醒快过期）
    accepted_cert: Option<Certificate>,
}

impl<'a> Authenticator<'a> {
    pub fn new(
        handle: &'a mut Handle<ClientHandler>,
        hop: &'a Hop,
        prompter: &'a Prompter,
        agent_path: Option<&'a str>,
    ) -> Self {
        Self {
            handle,
            hop,
            prompter,
            agent_path,
            totp_used: false,
            cert_problem: None,
            accepted_cert: None,
        }
    }

    /// 认证；成功时返回服务器接受了的证书（用证书登录时）。
    pub async fn run(mut self) -> Result<Option<Certificate>, SshError> {
        let plan = match &self.hop.auth {
            AuthPlan::Password { saved } if saved.is_some() => "password(saved)",
            AuthPlan::Password { .. } => "password(ask)",
            AuthPlan::Key { .. } => "key",
            AuthPlan::Agent => "agent",
            AuthPlan::AgentThenPassword => "agent-then-password",
        };
        tracing::debug!(
            host = %self.hop.address,
            user = %self.hop.username,
            plan,
            totp = self.hop.totp.is_some(),
            "开始认证"
        );
        let step = match self.hop.auth.clone() {
            AuthPlan::Password { saved } => self.password(saved, None).await?,
            AuthPlan::Key {
                key,
                passphrase,
                label,
                certificate,
            } => {
                self.key(&key, passphrase, &label, certificate.as_ref())
                    .await?
            }
            AuthPlan::Agent => self.agent().await?,
            AuthPlan::AgentThenPassword => match self.agent().await {
                Ok(Step::Success) => Step::Success,
                Ok(Step::Partial(methods)) => Step::Partial(methods),
                // 改问密码：之后的失败与 agent 中的证书无关
                Ok(Step::Failed(methods)) => {
                    self.cert_problem = None;
                    self.password(None, Some(methods)).await?
                }
                Err(_) => {
                    self.cert_problem = None;
                    self.password(None, None).await?
                }
            },
        };
        tracing::debug!(host = %self.hop.address, result = %step.describe(), "第一步认证的结果");
        match step {
            Step::Success => Ok(self.accepted_cert),
            // 第二步：验证码等
            Step::Partial(methods) => match self.second_factor(&methods).await? {
                Step::Success => Ok(self.accepted_cert),
                Step::Failed(methods) | Step::Partial(methods) => Err(SshError::AuthFailed {
                    methods: method_names(&methods),
                }),
            },
            Step::Failed(methods) => Err(match self.cert_problem {
                Some(problem) => {
                    tracing::info!(host = %self.hop.address, "证书没能用来登录：{problem}");
                    SshError::Certificate(problem)
                }
                None => SshError::AuthFailed {
                    methods: method_names(&methods),
                },
            }),
        }
    }

    async fn second_factor(&mut self, methods: &[MethodKind]) -> Result<Step, SshError> {
        if methods.contains(&MethodKind::KeyboardInteractive) {
            return self.keyboard_interactive(None).await;
        }
        if methods.contains(&MethodKind::Password) {
            let saved = match &self.hop.auth {
                AuthPlan::Password { saved } => saved.clone(),
                _ => None,
            };
            return self.password(saved, Some(methods.to_vec())).await;
        }
        Ok(Step::Failed(methods.to_vec()))
    }

    // ------------------------------------------------------------ 密码

    async fn password(
        &mut self,
        saved: Option<SecretString>,
        known_methods: Option<Vec<MethodKind>>,
    ) -> Result<Step, SshError> {
        let mut methods = known_methods;
        let mut candidate = saved;
        for attempt in 0..MAX_ATTEMPTS {
            let password = match candidate.take() {
                Some(password) => password,
                None => self.ask_password(attempt > 0).await?,
            };
            let allows = |kind: MethodKind| methods.as_ref().is_none_or(|m| m.contains(&kind));
            tracing::debug!(host = %self.hop.address, attempt = attempt + 1, "密码认证");
            let step = if allows(MethodKind::Password) {
                Step::from(
                    self.handle
                        .authenticate_password(&self.hop.username, password.expose())
                        .await?,
                )
            } else if allows(MethodKind::KeyboardInteractive) {
                self.keyboard_interactive(Some(&password)).await?
            } else {
                return Ok(Step::Failed(methods.unwrap_or_default()));
            };
            tracing::debug!(host = %self.hop.address, result = %step.describe(), "密码认证的结果");
            match step {
                Step::Failed(remaining) => {
                    // 服务器不接受密码方式而有键盘交互：用键盘交互回答同一个密码
                    if !remaining.contains(&MethodKind::Password)
                        && remaining.contains(&MethodKind::KeyboardInteractive)
                        && allows(MethodKind::Password)
                    {
                        methods = Some(remaining);
                        candidate = Some(password);
                        continue;
                    }
                    methods = Some(remaining);
                }
                other => return Ok(other),
            }
        }
        Ok(Step::Failed(methods.unwrap_or_default()))
    }

    async fn ask_password(&self, retry: bool) -> Result<SecretString, SshError> {
        match (self.prompter)(Prompt::Password {
            host: self.hop.label.clone(),
            username: self.hop.username.clone(),
            retry,
            endpoint: format!("{}:{}", self.hop.address, self.hop.port),
        })
        .await
        {
            Answer::Secret(password) => Ok(password),
            _ => Err(SshError::AuthCancelled),
        }
    }

    // ------------------------------------------------------------ 键盘交互

    async fn keyboard_interactive(
        &mut self,
        password: Option<&SecretString>,
    ) -> Result<Step, SshError> {
        tracing::debug!(host = %self.hop.address, with_password = password.is_some(), "键盘交互认证");
        let mut response = self
            .handle
            .authenticate_keyboard_interactive_start(&self.hop.username, None::<String>)
            .await?;
        let mut password_used = false;
        for _ in 0..MAX_ROUNDS {
            match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(Step::Success),
                KeyboardInteractiveAuthResponse::Failure {
                    remaining_methods,
                    partial_success,
                } => {
                    let methods = remaining_methods.to_vec();
                    return Ok(if partial_success {
                        Step::Partial(methods)
                    } else {
                        Step::Failed(methods)
                    });
                }
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => {
                    tracing::debug!(
                        host = %self.hop.address,
                        prompts = prompts.len(),
                        "服务器的键盘交互提问"
                    );
                    let answers = self
                        .answer_prompts(
                            &name,
                            &instructions,
                            &prompts,
                            password,
                            &mut password_used,
                        )
                        .await?;
                    response = self
                        .handle
                        .authenticate_keyboard_interactive_respond(answers)
                        .await?;
                }
            }
        }
        Ok(Step::Failed(Vec::new()))
    }

    /// 回答一轮键盘交互：密码类提示用已知的密码，验证码类提示用 TOTP，其余询问用户。
    async fn answer_prompts(
        &mut self,
        name: &str,
        instructions: &str,
        prompts: &[russh::client::Prompt],
        password: Option<&SecretString>,
        password_used: &mut bool,
    ) -> Result<Vec<String>, SshError> {
        if prompts.is_empty() {
            return Ok(Vec::new());
        }
        // 没有交出去的回答（用户取消、数量不对）随它清零；交给 russh 的只能是普通的 String（它的接口如此）
        let mut answers: zeroize::Zeroizing<Vec<Option<String>>> =
            zeroize::Zeroizing::new(vec![None; prompts.len()]);
        for (i, prompt) in prompts.iter().enumerate() {
            let text = prompt.prompt.to_lowercase();
            if !prompt.echo
                && is_password_prompt(&text)
                && !*password_used
                && let Some(password) = password
            {
                answers[i] = Some(password.expose().to_string());
                *password_used = true;
                continue;
            }
            if is_otp_prompt(&text)
                && !self.totp_used
                && let Some(code) = self.totp_code()
            {
                answers[i] = Some(code);
                self.totp_used = true;
            }
        }
        if answers.iter().all(Option::is_some) {
            tracing::debug!(host = %self.hop.address, "键盘交互自动回答（保存的密码或验证码）");
            return Ok(answers.iter_mut().filter_map(Option::take).collect());
        }
        tracing::debug!(host = %self.hop.address, "键盘交互需要用户回答");
        // 还有没法自动回答的：把整轮交给用户（已能回答的也一起显示，让用户知道在问什么）
        let asked = (self.prompter)(Prompt::KeyboardInteractive {
            host: self.hop.label.clone(),
            name: name.to_string(),
            instructions: instructions.to_string(),
            prompts: prompts.iter().map(|p| (p.prompt.clone(), p.echo)).collect(),
        })
        .await;
        match asked {
            Answer::Responses(responses) if responses.len() == prompts.len() => Ok(responses
                .into_iter()
                .enumerate()
                .map(|(i, r)| answers[i].take().unwrap_or_else(|| r.expose().to_string()))
                .collect()),
            _ => Err(SshError::AuthCancelled),
        }
    }

    fn totp_code(&self) -> Option<String> {
        let secret = self.hop.totp.as_ref()?;
        Totp::parse(secret.expose()).ok().map(|t| t.code_now())
    }

    // ------------------------------------------------------------ 私钥

    async fn key(
        &mut self,
        material: &KeyMaterial,
        passphrase: Option<SecretString>,
        label: &str,
        certificate: Option<&CertMaterial>,
    ) -> Result<Step, SshError> {
        tracing::debug!(
            host = %self.hop.address,
            key = %label,
            from_file = matches!(material, KeyMaterial::File(_)),
            "私钥认证"
        );
        let text = match material {
            KeyMaterial::Text(text) => text.clone(),
            KeyMaterial::File(path) => {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    std::fs::read_to_string(&path)
                        .map(SecretString::new)
                        .map_err(|e| {
                            SshError::Key(format!("读取私钥文件 {} 失败：{e}", path.display()))
                        })
                })
                .await
                .map_err(|e| SshError::Key(e.to_string()))??
            }
        };
        // OpenSSH、传统 PEM、PuTTY ppk 都认（见 sshkey）。解密要算 KDF（可能几百毫秒），放在阻塞线程上
        let mut passphrase = passphrase;
        for attempt in 0..=MAX_ATTEMPTS {
            let parsed = {
                let (text, passphrase) = (text.clone(), passphrase.clone());
                tokio::task::spawn_blocking(move || {
                    sshkey::parse(text.expose().as_bytes(), passphrase.as_ref())
                })
                .await
                .map_err(|e| SshError::Key(e.to_string()))?
            };
            match parsed {
                Ok(key) => {
                    let key = Arc::new(key.into_ssh_private_key());
                    let cert = match certificate {
                        Some(source) => self.load_certificate(source, &key).await,
                        None => None,
                    };
                    return self.publickey_with_certificate(key, cert).await;
                }
                // 加密的私钥没有密码、或密码不对：询问
                Err(error @ (KeyError::PassphraseRequired | KeyError::WrongPassphrase)) => {
                    tracing::debug!(key = %label, attempt = attempt + 1, "私钥需要密码：{error}");
                    if attempt == MAX_ATTEMPTS {
                        break;
                    }
                    let retry = passphrase.as_ref().is_some_and(|p| !p.is_empty());
                    passphrase = Some(self.ask_passphrase(label, &text, retry).await?);
                }
                Err(e) => {
                    tracing::warn!(key = %label, "解析私钥失败：{e}");
                    return Err(SshError::Key(e.to_string()));
                }
            }
        }
        Err(SshError::Key("私钥密码不正确".into()))
    }

    async fn ask_passphrase(
        &self,
        label: &str,
        text: &SecretString,
        retry: bool,
    ) -> Result<SecretString, SshError> {
        use sha2::Digest;
        let id = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(text.expose().as_bytes()));
        match (self.prompter)(Prompt::Passphrase {
            key: label.to_string(),
            retry,
            id,
        })
        .await
        {
            Answer::Secret(passphrase) => Ok(passphrase),
            _ => Err(SshError::AuthCancelled),
        }
    }

    async fn publickey(&mut self, key: Arc<PrivateKey>) -> Result<Step, SshError> {
        let hash = self.handle.best_supported_rsa_hash().await?.flatten();
        tracing::debug!(host = %self.hop.address, rsa_hash = ?hash, "用私钥认证");
        let key = PrivateKeyWithHashAlg::new(key, hash);
        let step: Step = self
            .handle
            .authenticate_publickey(&self.hop.username, key)
            .await?
            .into();
        tracing::debug!(host = %self.hop.address, result = %step.describe(), "私钥认证的结果");
        Ok(step)
    }

    // ------------------------------------------------------------ 证书

    /// 先出示证书；服务器不接受而还接受公钥时再用私钥本身。
    async fn publickey_with_certificate(
        &mut self,
        key: Arc<PrivateKey>,
        cert: Option<Certificate>,
    ) -> Result<Step, SshError> {
        let Some(cert) = cert else {
            return self.publickey(key).await;
        };
        let hash = self.handle.best_supported_rsa_hash().await?.flatten();
        tracing::debug!(
            host = %self.hop.address,
            certificate = %sshcert::summary(&cert),
            rsa_hash = ?hash,
            "用证书认证"
        );
        let result = self
            .handle
            .authenticate_certificate_with(
                &self.hop.username,
                cert.clone(),
                hash,
                &mut LocalSigner(key.clone()),
            )
            .await;
        let step = match result {
            Ok(result) => Step::from(result),
            Err(error) => {
                // 本机签名失败（几乎不会发生）：当作没出示证书
                tracing::warn!(host = %self.hop.address, "用证书签名失败：{error}");
                return self.publickey(key).await;
            }
        };
        tracing::debug!(host = %self.hop.address, result = %step.describe(), "证书认证的结果");
        match step {
            Step::Failed(methods) => {
                self.note_rejected(&cert);
                if !methods.is_empty() && !methods.contains(&MethodKind::PublicKey) {
                    return Ok(Step::Failed(methods));
                }
                tracing::debug!(host = %self.hop.address, "证书没有被接受，再用私钥本身");
                self.publickey(key).await
            }
            other => {
                self.accepted_cert = Some(cert);
                Ok(other)
            }
        }
    }

    /// 服务器没有接受证书：记下本机能看出的原因（过期、未生效、用户名），看不出时是“不信任 CA”。
    fn note_rejected(&mut self, cert: &Certificate) {
        let problem = sshcert::rejection(cert, &self.hop.username, sshcert::now_secs());
        tracing::info!(host = %self.hop.address, certificate = %sshcert::summary(cert), "服务器没有接受证书：{problem}");
        self.cert_problem.get_or_insert(problem);
    }

    /// 证书没能用上（读不了、对不上……）：记下原因，私钥照常尝试。
    fn note_unusable(&mut self, problem: CertProblem) {
        tracing::warn!(host = %self.hop.address, "不出示证书：{problem}");
        self.cert_problem.get_or_insert(problem);
    }

    /// 读取与私钥一起出示的证书：私钥文件旁没有证书时为 `None`；读不了、不是这把私钥的、是主机证书时记下原因，
    /// 也为 `None`。
    async fn load_certificate(
        &mut self,
        source: &CertMaterial,
        key: &PrivateKey,
    ) -> Option<Certificate> {
        // 显示用的名称：证书文件名；密钥库中的证书为空
        let (label, text) = match source {
            CertMaterial::Text(text) => (String::new(), Ok(text.clone())),
            CertMaterial::Beside(key_path) => {
                let key_path = key_path.clone();
                let found = tokio::task::spawn_blocking(move || {
                    sshcert::find_beside(&key_path).map(|path| (sshcert::read_file(&path), path))
                })
                .await
                .ok()
                .flatten();
                let (read, path) = found?;
                (file_label(&path), read.map_err(CertReadIssue::from))
            }
            CertMaterial::File(path) => {
                let read = {
                    let path = path.clone();
                    tokio::task::spawn_blocking(move || sshcert::read_file(&path))
                        .await
                        .map_err(|e| std::io::Error::other(e.to_string()))
                        .and_then(|r| r)
                };
                (file_label(path), read.map_err(CertReadIssue::from))
            }
        };
        let cert = match text.and_then(|t| sshcert::parse(&t).map_err(CertReadIssue::Invalid)) {
            Ok(cert) => cert,
            Err(reason) => {
                self.note_unusable(CertProblem::Unreadable {
                    certificate: label,
                    reason,
                });
                return None;
            }
        };
        if cert.cert_type() != russh::keys::ssh_key::certificate::CertType::User {
            self.note_unusable(CertProblem::HostCertificate { certificate: label });
            return None;
        }
        if !sshcert::matches_key(&cert, key.public_key().key_data()) {
            self.note_unusable(CertProblem::KeyMismatch { certificate: label });
            return None;
        }
        Some(cert)
    }

    // ------------------------------------------------------------ agent

    async fn agent(&mut self) -> Result<Step, SshError> {
        let mut agent = connect_agent(self.agent_path).await.inspect_err(|e| {
            tracing::debug!(host = %self.hop.address, "连不上 ssh-agent：{e}");
        })?;
        let identities = agent
            .request_identities()
            .await
            .map_err(|e| SshError::Key(format!("读取 agent 中的密钥失败：{e}")))?;
        tracing::debug!(host = %self.hop.address, keys = identities.len(), "用 ssh-agent 中的密钥认证");
        let mut last = Step::Failed(Vec::new());
        // 按 agent 列出的顺序（与 OpenSSH 相同）：普通的密钥与带证书的身份都试
        for identity in identities {
            let hash = self.handle.best_supported_rsa_hash().await?.flatten();
            let (step, cert) = match identity {
                AgentIdentity::PublicKey { key, .. } => {
                    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
                    let result = self
                        .handle
                        .authenticate_publickey_with(&self.hop.username, key, hash, &mut agent)
                        .await
                        .map_err(|e| SshError::Protocol(e.to_string()))?;
                    let step = Step::from(result);
                    tracing::debug!(
                        host = %self.hop.address,
                        key = %fingerprint,
                        result = %step.describe(),
                        "agent 密钥认证的结果"
                    );
                    (step, None)
                }
                AgentIdentity::Certificate { certificate, .. } => {
                    if certificate.cert_type() != russh::keys::ssh_key::certificate::CertType::User
                    {
                        continue;
                    }
                    let result = self
                        .handle
                        .authenticate_certificate_with(
                            &self.hop.username,
                            certificate.clone(),
                            hash,
                            &mut agent,
                        )
                        .await;
                    let step = match result {
                        Ok(result) => Step::from(result),
                        // agent 签不了这张证书（例如密钥已移除）：试下一个
                        Err(error) => {
                            tracing::debug!(host = %self.hop.address, "agent 用证书签名失败：{error}");
                            continue;
                        }
                    };
                    tracing::debug!(
                        host = %self.hop.address,
                        certificate = %sshcert::summary(&certificate),
                        result = %step.describe(),
                        "agent 证书认证的结果"
                    );
                    (step, Some(certificate))
                }
            };
            match step {
                Step::Failed(methods) => {
                    if let Some(cert) = &cert {
                        self.note_rejected(cert);
                    }
                    let continue_with_keys = methods.contains(&MethodKind::PublicKey);
                    last = Step::Failed(methods);
                    if !continue_with_keys {
                        break;
                    }
                }
                other => {
                    self.accepted_cert = cert;
                    return Ok(other);
                }
            }
        }
        Ok(last)
    }
}

/// 用本机的私钥替证书签名（russh 的 `authenticate_openssh_cert` 对 RSA 证书固定用 SHA-1 的算法名，
/// 不再接受 ssh-rsa 的服务器会拒绝；经这个签名者可以按服务器支持的 rsa-sha2-256 / 512 签名）。
struct LocalSigner(Arc<PrivateKey>);

#[derive(Debug, thiserror::Error)]
enum LocalSignError {
    #[error(transparent)]
    Send(#[from] russh::SendError),
    #[error("签名失败：{0}")]
    Sign(String),
}

impl russh::Signer for LocalSigner {
    type Error = LocalSignError;

    fn auth_sign(
        &mut self,
        _key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        let key = self.0.clone();
        async move {
            use russh::keys::signature::Signer as _;
            use russh::keys::ssh_encoding::Encode as _;
            use russh::keys::ssh_key::private::KeypairData;
            let fail = |e: &dyn std::fmt::Display| LocalSignError::Sign(e.to_string());
            let signature: russh::keys::ssh_key::Signature = match key.key_data() {
                KeypairData::Rsa(pair) => (pair, hash_alg).try_sign(&to_sign),
                data => data.try_sign(&to_sign),
            }
            .map_err(|e| fail(&e))?;
            // 与 agent 的回答相同：签名编码后作为一个 string 接在待签内容后面
            let encoded = signature.encode_vec().map_err(|e| fail(&e))?;
            encoded.encode(&mut to_sign).map_err(|e| fail(&e))?;
            Ok(to_sign)
        }
    }
}

/// 证书文件的显示名称（文件名）。
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// 连接 ssh-agent：指定了地址时用它；Windows 上依次试 `SSH_AUTH_SOCK`、系统的 OpenSSH agent、Pageant，
/// 其他平台用 `SSH_AUTH_SOCK`。
pub(crate) async fn connect_agent(
    path: Option<&str>,
) -> Result<
    AgentClient<Box<dyn russh::keys::agent::client::AgentStream + Send + Unpin + 'static>>,
    SshError,
> {
    let fail = |e: russh::keys::Error| SshError::Key(format!("无法连接 ssh-agent：{e}"));
    #[cfg(windows)]
    {
        let mut candidates: Vec<String> = Vec::new();
        if let Some(path) = path.filter(|p| !p.trim().is_empty()) {
            candidates.push(path.to_string());
        } else {
            if let Ok(sock) = std::env::var("SSH_AUTH_SOCK")
                && sock.starts_with(r"\\")
            {
                candidates.push(sock);
            }
            candidates.push(r"\\.\pipe\openssh-ssh-agent".to_string());
        }
        for candidate in &candidates {
            if let Ok(mut client) = AgentClient::connect_named_pipe(candidate).await {
                // 系统的 agent 在运行但没有密钥、而 Pageant 在运行时，改用 Pageant
                let has_keys = client
                    .request_identities()
                    .await
                    .map(|ids| !ids.is_empty())
                    .unwrap_or(false);
                if has_keys || path.is_some() {
                    return Ok(client.dynamic());
                }
            }
        }
        match AgentClient::connect_pageant().await {
            Ok(client) => Ok(client.dynamic()),
            Err(e) => Err(fail(e)),
        }
    }
    #[cfg(unix)]
    {
        let client = match path.filter(|p| !p.trim().is_empty()) {
            Some(path) => AgentClient::connect_uds(path).await,
            None => AgentClient::connect_env().await,
        }
        .map_err(fail)?;
        Ok(client.dynamic())
    }
}

/// 键盘交互中问密码的提示。
fn is_password_prompt(text: &str) -> bool {
    let text = text.to_lowercase();
    ["password", "密码", "passwort", "mot de passe", "contraseña"]
        .iter()
        .any(|word| text.contains(word))
}

/// 键盘交互中问验证码的提示（Google Authenticator、JumpServer、各家堡垒机的写法）。
fn is_otp_prompt(text: &str) -> bool {
    let text = text.to_lowercase();
    [
        "verification code",
        "verification_code",
        "one-time",
        "one time",
        "otp",
        "token",
        "authenticator",
        "2fa",
        "mfa",
        "totp",
        "code",
        "验证码",
        "动态",
        "口令",
        "令牌",
    ]
    .iter()
    .any(|word| text.contains(word))
        && !is_password_prompt(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_classification() {
        assert!(is_password_prompt("password: "));
        assert!(is_password_prompt("请输入密码："));
        assert!(is_otp_prompt("verification code: "));
        assert!(is_otp_prompt("[MFA auth]: "));
        assert!(is_otp_prompt("请输入 OTP 验证码："));
        assert!(!is_otp_prompt("password: "));
        assert!(!is_otp_prompt("Do you accept? "));
    }
}
