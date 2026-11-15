//! SSH 连接的错误：按用户能理解的原因分类，界面据此显示文案与下一步（重试、核对指纹、检查代理）。

/// 连接或认证失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    /// 域名解析失败
    #[error("无法解析地址 {host}：{reason}")]
    Dns { host: String, reason: String },
    /// 对方拒绝连接（端口没有服务）
    #[error("{host}:{port} 拒绝连接")]
    Refused { host: String, port: u16 },
    /// 连接超时
    #[error("连接 {host}:{port} 超时")]
    Timeout { host: String, port: u16 },
    /// 网络不可达等其他网络错误
    #[error("无法连接 {host}:{port}：{reason}")]
    Network {
        host: String,
        port: u16,
        reason: String,
    },
    /// 代理握手失败
    #[error("代理失败：{0}")]
    Proxy(String),
    /// 代理命令无法启动
    #[error("代理命令失败：{0}")]
    ProxyCommand(String),
    /// 代理命令不能使用：不认识的占位符，或代入的内容含有 shell 的特殊字符
    #[error("代理命令不能使用：{0}")]
    ProxyCommandInvalid(crate::ssh::proxy_command::CommandIssue),
    /// 代理命令在连接建立前退出了
    #[error("代理命令已退出（{}）{}", exit_text(.code), stderr_suffix(.stderr))]
    ProxyCommandExited {
        /// 退出码；被信号结束时为 `None`
        code: Option<i32>,
        /// 标准错误的最后几行
        stderr: String,
    },
    /// 代理命令还没有在这台电脑上确认（用户没有允许，或不能询问）
    #[error("代理命令还没有在这台电脑上确认")]
    ProxyCommandUnconfirmed,
    /// 用户拒绝了未知的主机指纹
    #[error("没有接受 {host} 的主机指纹")]
    HostKeyRejected { host: String },
    /// 主机指纹与已记录的不同（可能遭到中间人攻击，或服务器重装过）
    #[error("{host} 的主机指纹与记录的不符")]
    HostKeyChanged {
        host: String,
        port: u16,
        algorithm: String,
        /// 本次收到的指纹（SHA256:…）
        fingerprint: String,
        /// 记录中的指纹
        known_fingerprint: String,
    },
    /// 双方没有共同支持的算法（老设备只支持旧算法时提示开启"兼容旧设备"）
    #[error("没有双方都支持的算法：{0}")]
    NoCommonAlgorithm(String),
    /// 认证失败（密码错误、密钥不被接受等）
    #[error("认证失败（服务器支持：{methods}）")]
    AuthFailed { methods: String },
    /// 用证书登录失败：证书过期、尚未生效、用户名不在允许的列表中、服务器不信任签发它的 CA，
    /// 或证书与私钥对不上（私钥本身也没能登录）
    #[error("{0}")]
    Certificate(crate::sshcert::CertProblem),
    /// 用户取消了密码、验证码等询问
    #[error("已取消认证")]
    AuthCancelled,
    /// 需要交互（输入密码、验证码）但当前不能询问（命令行工具）
    #[error("需要输入密码或验证码，无法在这里询问")]
    InteractionRequired,
    /// 私钥无法读取或解析
    #[error("私钥不可用：{0}")]
    Key(String),
    /// 跳板机连接失败
    #[error("经跳板机 {jump} 连接失败：{reason}")]
    Jump { jump: String, reason: String },
    /// 跳板机拒绝转发到目标
    #[error("跳板机无法连到 {host}:{port}：{reason}")]
    JumpTarget {
        host: String,
        port: u16,
        reason: String,
    },
    /// 打开会话通道失败（例如堡垒机不允许另开通道）
    #[error("打开通道失败：{0}")]
    Channel(String),
    /// 连接已断开
    #[error("连接已断开")]
    Disconnected,
    /// 秘密读取失败（保险库锁定等）
    #[error("读取保存的凭据失败：{0}")]
    Vault(String),
    /// 主机配置有误（跳板链循环、引用的主机不存在等）
    #[error("主机配置有误：{0}")]
    Config(String),
    /// 其他协议错误
    #[error("SSH 错误：{0}")]
    Protocol(String),
}

fn exit_text(code: &Option<i32>) -> String {
    match code {
        Some(code) => format!("退出码 {code}"),
        None => "被信号结束".into(),
    }
}

fn stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!("：{stderr}")
    }
}

impl SshError {
    /// 网络错误的分类（连接 TCP 时）。
    pub(crate) fn from_io(host: &str, port: u16, error: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::ConnectionRefused => SshError::Refused {
                host: host.into(),
                port,
            },
            ErrorKind::TimedOut => SshError::Timeout {
                host: host.into(),
                port,
            },
            _ => SshError::Network {
                host: host.into(),
                port,
                reason: error.to_string(),
            },
        }
    }

    /// 自动重连是否值得一试：认证、指纹、配置问题重试也没用。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SshError::Refused { .. }
                | SshError::Timeout { .. }
                | SshError::Network { .. }
                | SshError::Dns { .. }
                | SshError::Disconnected
                | SshError::Proxy(_)
                | SshError::ProxyCommandExited { .. }
                | SshError::Jump { .. }
                | SshError::JumpTarget { .. }
                | SshError::Protocol(_)
        )
    }
}

impl From<russh::Error> for SshError {
    fn from(error: russh::Error) -> Self {
        match error {
            russh::Error::Disconnect | russh::Error::HUP => SshError::Disconnected,
            russh::Error::NoCommonAlgo { kind, ours, theirs } => {
                SshError::NoCommonAlgorithm(format!(
                    "{kind:?}：本机 {}；服务器 {}",
                    ours.join(", "),
                    theirs.join(", ")
                ))
            }
            other => SshError::Protocol(other.to_string()),
        }
    }
}
