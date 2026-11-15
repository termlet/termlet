//! 连接过程中需要用户回答的问题：主机指纹、密码、私钥密码、键盘交互（验证码等）、执行代理命令。
//!
//! 核心不知道界面：调用方给出一个 [`Prompter`]，桌面端把问题交给窗口弹出对话框，命令行工具一律回答"不能询问"。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::secret::SecretString;

/// 一次询问。
#[derive(Clone, Debug)]
pub enum Prompt {
    /// 第一次连接这台主机：请确认指纹
    HostKey(HostKeyPrompt),
    /// 需要密码（没有保存，或保存的不对）
    Password {
        /// 显示用的主机名称
        host: String,
        username: String,
        /// 上一次输入的密码不对
        retry: bool,
        /// 记住密码用的标识（`地址:端口`，不显示）：显示名称可能重名
        endpoint: String,
    },
    /// 私钥被加密，需要私钥密码
    Passphrase {
        /// 显示用的私钥名称
        key: String,
        retry: bool,
        /// 记住私钥密码用的标识（私钥内容的哈希，不显示）：不同的私钥可能同名
        id: String,
    },
    /// 服务器的键盘交互认证（验证码、二次确认等）
    KeyboardInteractive {
        host: String,
        name: String,
        instructions: String,
        /// 每个提示的文字与是否回显（不回显的是密码类）
        prompts: Vec<(String, bool)>,
    },
    /// 这台电脑上第一次执行这条代理命令：请确认（见 [`crate::ssh::proxy_command`]）
    ProxyCommand(ProxyCommandPrompt),
}

/// 确认执行代理命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyCommandPrompt {
    /// 显示用的主机名称
    pub host: String,
    /// 替换过占位符、将要执行的完整命令
    pub command: String,
    /// 设置中的原文（确认记在它上面：用同一条命令的其他主机不再询问）
    pub template: String,
    /// 命令沿用自哪个分组；主机自己设置的为 `None`
    pub from_group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub host: String,
    pub port: u16,
    /// 算法（ssh-ed25519、ecdsa-sha2-nistp256……）
    pub algorithm: String,
    /// SHA256 指纹
    pub fingerprint: String,
    /// 这台主机已记录的其他算法：不为空时服务器没有提供记录过的那种密钥，可能是有人冒充，要醒目地提醒
    pub known_algorithms: Vec<String>,
}

/// 用户的回答。
#[derive(Debug)]
pub enum Answer {
    /// 接受指纹（并记住）
    AcceptHostKey,
    /// 允许执行代理命令（并记在这台电脑上）
    AllowProxyCommand,
    /// 拒绝、取消
    Cancel,
    /// 输入的密码或私钥密码
    Secret(SecretString),
    /// 键盘交互的各项回答
    Responses(Vec<SecretString>),
}

pub type PromptFuture = Pin<Box<dyn Future<Output = Answer> + Send>>;

/// 回答问题的一方。
pub type Prompter = Arc<dyn Fn(Prompt) -> PromptFuture + Send + Sync>;

/// 不能询问（命令行工具、后台任务）：一律取消。
pub fn no_prompt() -> Prompter {
    Arc::new(|_| Box::pin(async { Answer::Cancel }))
}

/// 记住用户输入过的密码与私钥密码：同一个 Tab 断线重连、复制会话时不再询问。密码按"用户名@地址:端口"、
/// 私钥密码按私钥内容记，同名的不同主机、私钥不会互相顶替。
/// 只在内存中，随返回的发问者一起丢弃（清零）。记住的不对时（再次询问且 `retry` 为真）忘掉它、照常询问。
pub fn remembering(inner: Prompter) -> Prompter {
    let remembered: Arc<Mutex<HashMap<String, SecretString>>> = Arc::default();
    Arc::new(move |prompt| {
        let inner = inner.clone();
        let remembered = remembered.clone();
        Box::pin(async move {
            let key = match &prompt {
                Prompt::Password {
                    username, endpoint, ..
                } => Some(format!("password:{username}@{endpoint}")),
                Prompt::Passphrase { id, .. } => Some(format!("passphrase:{id}")),
                _ => None,
            };
            let retry = matches!(
                prompt,
                Prompt::Password { retry: true, .. } | Prompt::Passphrase { retry: true, .. }
            );
            let Some(key) = key else {
                return inner(prompt).await;
            };
            if retry {
                remembered.lock().unwrap().remove(&key);
            } else if let Some(secret) = remembered.lock().unwrap().get(&key) {
                return Answer::Secret(secret.clone());
            }
            let answer = inner(prompt).await;
            if let Answer::Secret(secret) = &answer {
                remembered.lock().unwrap().insert(key, secret.clone());
            }
            answer
        })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn password(retry: bool) -> Prompt {
        password_for("10.0.0.1:22", retry)
    }

    fn password_for(endpoint: &str, retry: bool) -> Prompt {
        Prompt::Password {
            host: "web".into(),
            username: "root".into(),
            retry,
            endpoint: endpoint.into(),
        }
    }

    #[tokio::test]
    async fn remembering_reuses_password_until_it_is_wrong() {
        let asked = Arc::new(AtomicUsize::new(0));
        let inner: Prompter = {
            let asked = asked.clone();
            Arc::new(move |_| {
                let n = asked.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Answer::Secret(SecretString::new(format!("p{n}"))) })
            })
        };
        let prompter = remembering(inner);
        let secret = |answer: Answer| match answer {
            Answer::Secret(s) => s.expose().to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(secret(prompter(password(false)).await), "p0");
        // 重连：直接用记住的
        assert_eq!(secret(prompter(password(false)).await), "p0");
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // 记住的不对：重新询问并记住新的
        assert_eq!(secret(prompter(password(true)).await), "p1");
        assert_eq!(secret(prompter(password(false)).await), "p1");
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        // 同名（显示名称、用户名都相同）的另一台主机：另外询问
        assert_eq!(
            secret(prompter(password_for("10.0.0.2:22", false)).await),
            "p2"
        );
        assert_eq!(secret(prompter(password(false)).await), "p1");
        assert_eq!(asked.load(Ordering::SeqCst), 3);
        // 指纹等其他问题不记
        prompter(Prompt::HostKey(HostKeyPrompt {
            host: "web".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:x".into(),
            known_algorithms: Vec::new(),
        }))
        .await;
        assert_eq!(asked.load(Ordering::SeqCst), 4);
    }
}
