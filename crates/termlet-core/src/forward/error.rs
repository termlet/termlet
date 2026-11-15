//! 端口转发的错误：按界面要写的文案分类。
//!
//! 消息是写给日志看的中文；界面按变体选用自己的文案。错误中只有地址、端口与服务器的说明，没有秘密。

use crate::ssh::SshError;

/// 启动转发失败、或运行中因错误停止的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForwardError {
    /// 规则不存在（已被删除）
    #[error("转发规则不存在")]
    RuleNotFound,
    /// 规则经由的主机不存在
    #[error("转发经由的主机不存在")]
    HostNotFound,
    /// 规则经由的主机不是 SSH 主机（改成了 Telnet、串口）
    #[error("转发经由的主机不是 SSH 主机")]
    NotSsh,
    /// 本机的监听地址已被别的程序（或另一条规则）占用
    #[error("本地地址 {address} 已被占用")]
    PortInUse { address: String },
    /// 没有权限监听：Windows 上被 Hyper-V 等保留的端口段、Unix 上 1024 以下的端口
    #[error("无法监听 {address}（端口被系统保留或没有权限）")]
    ListenDenied { address: String },
    /// 本机没有这个地址（例如系统关闭了 IPv6 时的 ::1）
    #[error("本机没有地址 {address}")]
    AddressUnavailable { address: String },
    /// 其他监听失败
    #[error("无法监听 {address}：{reason}")]
    Listen { address: String, reason: String },
    /// 远程转发：服务器拒绝监听（端口已被占用、低于 1024 而不是 root、sshd 禁止了转发）
    #[error("服务器拒绝监听 {address}（端口可能已被占用、低于 1024，或 sshd 禁止了端口转发）")]
    RemoteListenDenied { address: String },
    /// 主机设置了"只用一个通道"（堡垒机）：不能与终端共用连接
    #[error("这台主机只用一个通道，不能共用终端的连接")]
    SingleChannel,
    /// 连接或认证失败（重连时需要输入密码、验证码的是 [`SshError::AuthCancelled`]）
    #[error("{0}")]
    Connect(#[from] SshError),
    /// 启动期间被停止（用户点了停止、窗口关闭）
    #[error("转发已被停止")]
    Stopped,
}

/// 运行期间某个连接没能接到目标的原因。转发本身照常运行，界面显示最近一次的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    /// 服务器连不上目标：目标端口没有服务、地址不可达或域名解析失败（服务器报告的是同一种错误，分不出来）
    #[error("服务器无法连接 {target}")]
    Unreachable { target: String },
    /// 服务器不允许转发到这个目标（sshd 的 AllowTcpForwarding、PermitOpen）
    #[error("服务器不允许转发到 {target}")]
    Prohibited { target: String },
    /// 打开通道失败或超时（服务器的通道数已满、连接没有响应等）
    #[error("经服务器连接 {target} 失败：{reason}")]
    Channel { target: String, reason: String },
    /// 远程转发：本机的目标拒绝连接（没有服务在监听）
    #[error("本机的 {target} 拒绝连接")]
    LocalRefused { target: String },
    /// 远程转发：连接本机的目标失败（超时、解析不了等）
    #[error("连接本机的 {target} 失败：{reason}")]
    Local { target: String, reason: String },
    /// 动态转发：客户端的 SOCKS 请求有问题
    #[error("{0}")]
    Socks(#[from] SocksError),
}

/// 动态转发中 SOCKS 握手失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SocksError {
    /// 客户端在握手完成前断开
    #[error("读取 SOCKS 请求失败")]
    Read,
    /// 握手（含等待重连）超时
    #[error("SOCKS 握手超时")]
    Timeout,
    /// 收到的不是 SOCKS 请求（例如把动态转发当成了 HTTP 代理）
    #[error("收到的不是 SOCKS 请求，请在客户端把代理类型设为 SOCKS5")]
    NotSocks,
    /// 客户端只提供用户名密码认证
    #[error("客户端要求用户名密码认证，动态转发不需要认证，请在客户端去掉代理的用户名密码")]
    AuthRequired,
    /// 只支持 CONNECT（BIND、UDP ASSOCIATE 不支持）
    #[error("不支持的 SOCKS 命令 {0}，只支持 CONNECT")]
    UnsupportedCommand(u8),
    /// 不认识的地址类型
    #[error("不支持的 SOCKS 地址类型 {0}")]
    UnsupportedAddress(u8),
    /// 域名为空或含控制字符
    #[error("SOCKS 请求中的域名格式不正确")]
    BadDomain,
    /// 其他格式错误
    #[error("SOCKS 请求格式错误")]
    Malformed,
}
