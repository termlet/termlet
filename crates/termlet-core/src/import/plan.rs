//! 导入计划：与来源无关的解析结果。

use std::collections::{HashMap, HashSet};

use crate::id::{HostId, KeyId, ProfileId};
use crate::model::{Auth, Encoding, Forward, Host, Jump, KeySource, Snippet};
use crate::secret::SecretString;

use super::Source;

/// 一次解析的结果：要导入的分组、主机与密钥。
///
/// 计划只用自己内部的下标互相引用，写入存储时再换成真正的 ID：
/// - 分组按"上级在前"的顺序排列，`parent` 总是指向更靠前的分组；
/// - 主机的跳板机在计划内时是主机的下标，否则按地址、端口、用户名在已有主机中查找（见 [`JumpRef`]）；
/// - 用密钥库中私钥的主机引用 `keys` 的下标（见 [`PlannedHost::key`]）；
/// - 用凭据档案的主机引用 `profiles` 的下标（见 [`PlannedHost::profile`]），档案用私钥时也引用 `keys`。
#[derive(Clone, Debug)]
pub struct ImportPlan {
    pub source: Source,
    pub groups: Vec<PlannedGroup>,
    pub hosts: Vec<PlannedHost>,
    /// 要存进密钥库的私钥（Termora、Electerm、NyaTerm 等）
    pub keys: Vec<PlannedKey>,
    /// 凭据档案（多台主机共用的用户名与认证，如 NyaTerm 的账号）：随用到它的勾选主机一起导入
    pub profiles: Vec<PlannedProfile>,
    /// 整个计划的说明（不针对某台主机），预览中各写一行
    pub notes: Vec<PlanNote>,
    /// 快捷命令（WindTerm、NyaTerm）：随勾选的主机一起导入（NyaTerm 的文件中只有快捷命令），与已有的
    /// （名称、内容都相同）不重复导入
    pub snippets: Vec<Snippet>,
    /// 快捷命令转换时丢掉或改动的内容（NyaTerm），预览中说明
    pub snippet_notes: SnippetNotes,
    /// 来源中跳过的会话数：RDP、VNC 等不支持的会话，缺少地址的会话，以及无法解密的记录；NyaTerm 为跳过的快捷命令数
    pub skipped: usize,
}

/// 快捷命令从其他程序的写法转换过来时丢掉或改动的内容：各项是涉及的快捷命令条数（NyaTerm）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnippetNotes {
    /// 去掉了暂停（`\p`）：Termlet 没有暂停，导入后连续发送
    pub pauses: usize,
    /// 变量的下拉选项只保留了第一个，作为默认值
    pub options: usize,
    /// 有 Termlet 认不出的变量（名称中有 `/`、`:` 等），按原文保留
    pub unknown_params: usize,
    /// 原样发送（不替换变量）的命令中有 `{{…}}`，在 Termlet 中会当作参数填写
    pub literal_params: usize,
    /// 设置了逐行、逐字发送：间隔不保留，导入后一次发送
    pub paced: usize,
    /// 有说明：不导入
    pub descriptions: usize,
    /// 有颜色、图标、置顶或风险等级：不导入
    pub decorations: usize,
    /// 在嵌套的分类中：分组合成一层，写成“上级 / 下级”
    pub nested_groups: usize,
}

impl SnippetNotes {
    /// 没有丢掉或改动任何内容。
    pub fn is_empty(&self) -> bool {
        *self == SnippetNotes::default()
    }
}

impl ImportPlan {
    pub(super) fn new(source: Source) -> Self {
        Self {
            source,
            groups: Vec::new(),
            hosts: Vec::new(),
            keys: Vec::new(),
            profiles: Vec::new(),
            notes: Vec::new(),
            snippets: Vec::new(),
            snippet_notes: SnippetNotes::default(),
            skipped: 0,
        }
    }

    /// 计划中有来源保存的秘密（登录密码、私钥密码、代理密码、验证码密钥或私钥，主机或凭据档案的）。
    pub fn has_secrets(&self) -> bool {
        !self.keys.is_empty()
            || self.profiles.iter().any(PlannedProfile::has_secrets)
            || self.hosts.iter().any(|h| {
                h.has_password()
                    || h.has_proxy_password()
                    || h.passphrase().is_some()
                    || h.totp().is_some()
            })
    }

    /// 去掉来源中的秘密（用户没有同意导入保存的密码时，见 [`Source::secrets_need_consent`]）：主机照常导入，
    /// 用来源中私钥的主机与凭据档案改为密码认证，连接时再输入。
    pub fn without_secrets(&self) -> ImportPlan {
        let mut plan = self.clone();
        plan.keys.clear();
        for host in &mut plan.hosts {
            host.credentials = Credentials::default();
        }
        for profile in &mut plan.profiles {
            profile.credentials = ProfileCredentials::default();
            if matches!(profile.auth, PlannedProfileAuth::Key { .. }) {
                profile.auth = PlannedProfileAuth::Password;
            }
        }
        plan
    }

    /// 主机实际使用的用户名：自己的，没有时为凭据档案的。
    pub fn username_of(&self, host: &PlannedHost) -> String {
        if !host.host.username.trim().is_empty() {
            return host.host.username.clone();
        }
        host.profile
            .and_then(|p| self.profiles.get(p))
            .map(|p| p.username.clone())
            .unwrap_or_default()
    }

    /// 分组从根开始的各级名称。
    pub fn group_path(&self, group: usize) -> Vec<&str> {
        let mut names = Vec::new();
        let mut current = Some(group);
        while let Some(i) = current {
            let Some(g) = self.groups.get(i) else { break };
            names.push(g.name.as_str());
            // 上级总在前面，下标严格变小，不会循环
            current = g.parent.filter(|&p| p < i);
        }
        names.reverse();
        names
    }
}

/// 一个分组（来源中的文件夹）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedGroup {
    pub name: String,
    /// 上级分组（`ImportPlan::groups` 的下标），`None` 表示在根上
    pub parent: Option<usize>,
}

/// 一台待导入的主机。
#[derive(Clone, Debug)]
pub struct PlannedHost {
    /// 在本次导入中唯一的键（来源中的相对路径、别名或 ID），界面勾选时用
    pub key: String,
    /// 能直接映射的内容：名称、地址、端口、用户名、认证（私钥文件、ssh-agent，其余为密码）、代理、标签、备注。
    /// 跳板机保持默认值，由 [`PlannedHost::jump`] 在写入时设置；用密钥库中的私钥时认证也在写入时设置
    /// （见 [`PlannedHost::to_host`]）
    pub host: Host,
    /// 所在分组（`ImportPlan::groups` 的下标），`None` 表示不分组
    pub group: Option<usize>,
    /// 跳板机，`None` 表示直连
    pub jump: Option<JumpRef>,
    /// 需要提醒用户的事项
    pub hints: Vec<Hint>,
    /// 随主机导入的端口转发（Xshell 的隧道；`host` 在写入时设置）：导入后不自动启动，合并时添加到已有的主机上，
    /// 跳过已有的主机时不导入
    pub forwards: Vec<Forward>,
    /// 使用的凭据档案（`ImportPlan::profiles` 的下标）：用户名与认证由档案提供，主机自己的用户名优先
    profile: Option<usize>,
    /// 档案的用户名（主机没有自己的用户名时用于判断重复）
    profile_username: String,
    credentials: Credentials,
}

/// 来源中保存的凭据（Termora、WindTerm、NyaTerm 等）。只有核心能取出明文。
#[derive(Clone, Debug, Default)]
struct Credentials {
    password: Option<SecretString>,
    /// 私钥文件的密码（主机用私钥文件时保存）
    passphrase: Option<SecretString>,
    proxy_password: Option<SecretString>,
    /// 验证码（TOTP）的密钥：Base32 或 `otpauth://` 链接（NyaTerm）
    totp: Option<SecretString>,
    /// `ImportPlan::keys` 的下标
    key: Option<usize>,
}

impl PlannedHost {
    pub(super) fn new(key: impl Into<String>, host: Host) -> Self {
        Self {
            key: key.into(),
            host,
            group: None,
            jump: None,
            hints: Vec::new(),
            forwards: Vec::new(),
            profile: None,
            profile_username: String::new(),
            credentials: Credentials::default(),
        }
    }

    /// 判断重复用的键：用户名是主机自己的，没有时为凭据档案的。
    pub fn endpoint(&self) -> Endpoint {
        let username = if self.host.username.trim().is_empty() {
            &self.profile_username
        } else {
            &self.host.username
        };
        Endpoint::new(&self.host.address, self.host.effective_port(), username)
    }

    /// 使用的凭据档案（`ImportPlan::profiles` 的下标）。
    pub fn profile(&self) -> Option<usize> {
        self.profile
    }

    /// 改用凭据档案（`index` 是 [`PlanBuilder::add_profile`] 返回的下标，`username` 是档案的用户名）：主机自己的
    /// 用户名留空时用档案的。
    pub(super) fn set_profile(&mut self, index: usize, username: &str) {
        self.profile = Some(index);
        self.profile_username = username.trim().to_string();
    }

    /// 来源中保存了登录密码，会一并导入。
    pub fn has_password(&self) -> bool {
        self.credentials.password.is_some()
    }

    /// 来源中保存了代理的密码，会一并导入。
    pub fn has_proxy_password(&self) -> bool {
        self.credentials.proxy_password.is_some()
    }

    /// 来源中保存了私钥，会存进密钥库（`ImportPlan::keys` 的下标）。
    pub fn key(&self) -> Option<usize> {
        self.credentials.key
    }

    /// 登录密码（只有核心能取出，界面只看 [`PlannedHost::has_password`]）。
    pub(crate) fn password(&self) -> Option<&SecretString> {
        self.credentials.password.as_ref()
    }

    /// 代理的密码。
    pub(crate) fn proxy_password(&self) -> Option<&SecretString> {
        self.credentials.proxy_password.as_ref()
    }

    /// 私钥文件的密码。
    pub(crate) fn passphrase(&self) -> Option<&SecretString> {
        self.credentials.passphrase.as_ref()
    }

    /// 验证码的密钥。
    pub(crate) fn totp(&self) -> Option<&SecretString> {
        self.credentials.totp.as_ref()
    }

    pub(super) fn set_password(&mut self, password: SecretString) {
        self.credentials.password = Some(password).filter(|p| !p.is_empty());
    }

    pub(super) fn set_passphrase(&mut self, passphrase: SecretString) {
        self.credentials.passphrase = Some(passphrase).filter(|p| !p.is_empty());
    }

    /// 终端编码：`name` 是来源中的写法；Termlet 不支持的（`encoding` 为 `None`）按 UTF-8 并提醒。
    pub(super) fn set_encoding(&mut self, encoding: Option<Encoding>, name: &str) {
        match encoding {
            Some(encoding) => self.host.terminal.encoding = encoding,
            None => self.hints.push(Hint::UnsupportedEncoding {
                name: name.trim().to_string(),
            }),
        }
    }

    pub(super) fn set_proxy_password(&mut self, password: SecretString) {
        self.credentials.proxy_password = Some(password).filter(|p| !p.is_empty());
    }

    pub(super) fn set_totp(&mut self, secret: SecretString) {
        self.credentials.totp = Some(secret).filter(|s| !s.is_empty());
    }

    pub(super) fn set_key(&mut self, key: usize) {
        self.credentials.key = Some(key);
    }

    /// 写入时的主机内容：`key` 是私钥存进密钥库后的 ID（没有存进去时传 `None`，主机改用密码认证），
    /// `jump` 是跳板机的 ID，`profile` 是凭据档案的 ID（没能导入时传 `None`，主机改用密码认证）。
    pub fn to_host(
        &self,
        key: Option<KeyId>,
        jump: Option<HostId>,
        profile: Option<ProfileId>,
    ) -> Host {
        let mut host = self.host.clone();
        if let (Some(key), Some(_)) = (key, self.credentials.key) {
            host.auth = Auth::Key {
                source: KeySource::Library { key },
            };
        }
        if let (Some(profile), Some(_)) = (profile, self.profile) {
            host.auth = Auth::Profile { profile };
        }
        if let Some(jump) = jump {
            host.jump = Jump::Via { host: jump };
        }
        host
    }
}

/// 要导入的凭据档案。写入时本机已有名称与用户名都相同的档案就直接用它（不改动它），否则新建（与主机文件的
/// 导入相同）。
#[derive(Clone, Debug)]
pub struct PlannedProfile {
    pub name: String,
    pub username: String,
    pub auth: PlannedProfileAuth,
    credentials: ProfileCredentials,
}

/// 凭据档案的认证。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedProfileAuth {
    /// 密码（来源中保存了时一并导入）
    Password,
    /// 计划中的私钥（`ImportPlan::keys` 的下标），写入时设为密钥库中的密钥；没能存进密钥库时改为密码
    Key { key: usize },
    /// 本机的私钥文件（私钥密码保存了时一并导入）
    KeyFile { path: String },
    /// ssh-agent / Pageant
    Agent,
}

/// 凭据档案中来源保存的秘密。只有核心能取出明文。
#[derive(Clone, Debug, Default)]
struct ProfileCredentials {
    password: Option<SecretString>,
    passphrase: Option<SecretString>,
}

impl PlannedProfile {
    /// 名称、用户名截到存储的长度上限；名称为空时用用户名。
    pub(super) fn new(name: &str, username: &str, auth: PlannedProfileAuth) -> Self {
        let username = clip(username.trim(), 128);
        let name = match clip(name.trim(), 128) {
            n if n.is_empty() => username.clone(),
            n => n,
        };
        Self {
            name,
            username,
            auth,
            credentials: ProfileCredentials::default(),
        }
    }

    /// 来源中保存了档案的密码（密码认证时）。
    pub fn has_password(&self) -> bool {
        self.auth == PlannedProfileAuth::Password && self.credentials.password.is_some()
    }

    /// 档案用计划中的私钥（存进密钥库的下标）。
    pub fn key(&self) -> Option<usize> {
        match self.auth {
            PlannedProfileAuth::Key { key } => Some(key),
            _ => None,
        }
    }

    fn has_secrets(&self) -> bool {
        self.credentials.password.is_some() || self.credentials.passphrase.is_some()
    }

    pub(crate) fn password(&self) -> Option<&SecretString> {
        self.credentials.password.as_ref()
    }

    pub(crate) fn passphrase(&self) -> Option<&SecretString> {
        self.credentials.passphrase.as_ref()
    }

    pub(super) fn set_password(&mut self, password: SecretString) {
        self.credentials.password = Some(password).filter(|p| !p.is_empty());
    }

    /// 私钥文件的密码（[`PlannedProfileAuth::KeyFile`] 时）。现在的来源（NyaTerm 的账号）还没有用到，留给 ShellRS 等
    /// 有私钥文件档案的来源。
    #[allow(dead_code)]
    pub(super) fn set_passphrase(&mut self, passphrase: SecretString) {
        self.credentials.passphrase = Some(passphrase).filter(|p| !p.is_empty());
    }
}

/// 整个计划的说明（不针对某台主机），界面按变体选用自己的文案。各来源按需要加变体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanNote {
    /// 来源中加密保存的秘密都解不开（NyaTerm 的本机数据来自另一台电脑，或用户目录改过名）：主机照常导入、
    /// 不带密码与私钥，连接时再输入；可以改用来源导出的备份
    SecretsUnreadable,
    /// 读的是不含秘密的数据：来源把密码、私钥密码、验证码密钥保存在系统钥匙串中，没有导入，连接时再输入
    /// （ShellRS 的数据目录）
    SecretsInKeychain,
    /// 来源开启了同步，同步文件夹 `path` 中有带秘密的数据：改选它并输入同步密码可以一并导入密码与私钥（ShellRS）
    SyncFolderAvailable { path: std::path::PathBuf },
    /// 来源经 WebDAV、S3 同步：要一并导入密码与私钥，先把同步数据下载到本机再选（ShellRS）
    SyncFolderRemote,
    /// 读的是同步数据：是各设备最后一次同步时的内容，不在同步中的（串口主机）要改选数据目录（ShellRS）
    SyncSnapshot,
}

/// 判断重复用的键：地址（不区分大小写）、端口、用户名。三项都相同的主机视为同一台，导入时可以选择合并或跳过。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    address: String,
    port: u16,
    username: String,
}

impl Endpoint {
    /// `port` 为 0 时按 SSH 的 22。已有主机的用户名沿用分组或凭据档案时，`username` 传沿用到的用户名。
    pub fn new(address: &str, port: u16, username: &str) -> Self {
        Self {
            address: address.trim().to_lowercase(),
            port: if port == 0 { 22 } else { port },
            username: username.trim().to_string(),
        }
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn username(&self) -> &str {
        &self.username
    }
}

/// 跳板机。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpRef {
    /// 跳板机在来源中的名称（提示与界面显示用）
    pub name: String,
    pub endpoint: Endpoint,
    /// 跳板机就在本次导入中时为它的下标（`ImportPlan::hosts`）。没有下标、或那台主机没有勾选导入时，
    /// 写入时按 `endpoint` 在已有主机中查找，找不到（或会形成循环）时直连
    pub host: Option<usize>,
}

impl JumpRef {
    pub(super) fn new(name: impl Into<String>, endpoint: Endpoint) -> Self {
        Self {
            name: name.into(),
            endpoint,
            host: None,
        }
    }
}

/// 要存进密钥库的私钥。
#[derive(Clone, Debug)]
pub struct PlannedKey {
    /// 在来源中的名称
    pub name: String,
    pub algorithm: KeyAlgorithm,
    /// 不加密的 PKCS#8 PEM（`-----BEGIN PRIVATE KEY-----`）
    private_key: SecretString,
}

impl PlannedKey {
    pub(super) fn new(name: String, algorithm: KeyAlgorithm, private_key: SecretString) -> Self {
        Self {
            name,
            algorithm,
            private_key,
        }
    }

    /// 不加密的 PKCS#8 PEM。存进密钥库前要转为 OpenSSH 格式，并算出公钥与指纹。
    pub(crate) fn private_key(&self) -> &SecretString {
        &self.private_key
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAlgorithm {
    Rsa,
    Ecdsa,
    Ed25519,
}

/// 需要提醒用户的事项。界面按变体选用自己的文案，来源的名称见 [`ImportPlan::source`]。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    /// 来源中没有用户名，导入后需要补充
    MissingUsername,
    /// 私钥保存在来源自己的密钥库中（Xshell 的用户密钥、FinalShell、Tabby 的 Vault），需要重新选择私钥文件；
    /// `name` 是它在来源中的名称
    PrivateKeyNotImported { name: Option<String> },
    /// 来源中设为私钥认证，但没有指定私钥文件（Tabby）
    PrivateKeyFileMissing,
    /// 来源中配置了 `total` 个私钥，只导入了第一个（Tabby）
    ExtraPrivateKeys { total: usize },
    /// 来源中使用的密钥已不存在，需要重新选择私钥（Termora）
    LibraryKeyMissing,
    /// 来源中的密钥 `name` 无法导入（格式不支持或已损坏），需要重新选择私钥（Termora）
    LibraryKeyUnusable { name: String },
    /// 来源中的密钥 `name` 附带的 OpenSSH 证书没有导入，需要时在密钥库中为它添加（NyaTerm）
    CertificateNotImported { name: String },
    /// 私钥路径中有无法识别的占位符，导入后需要确认私钥路径（MobaXterm）
    UnknownPathPlaceholder { placeholder: String },
    /// 来源中经多级跳板机连接，只导入了最后一跳作为跳板机，需要检查那台跳板机自己的跳板设置。
    /// `chain` 是依次经过的跳板机（最后一个是导入的跳板机）
    MultiHopJump { chain: Vec<String> },
    /// 来源中引用的跳板机不存在，导入后直连（Tabby、Termora）
    JumpNotFound,
    /// 跳板机 `name` 没有用户名，导入后需要手动设置跳板机（Tabby、MobaXterm 的 SSH 网关）
    JumpMissingUsername { name: String },
    /// 同时配置了 ProxyJump 与 ProxyCommand，只导入了 ProxyJump（ssh_config）
    ProxyCommandIgnored,
    /// 导入了代理命令，连接时会在本机执行（命令见主机的代理设置）
    ProxyCommandImported,
    /// 来源中没有设置代理端口，按 `port` 导入，需要确认
    ProxyPortDefaulted { port: u16 },
    /// 代理需要密码，导入后在主机的代理设置中填写
    ProxyPasswordRequired,
    /// 来源中的代理类型不支持，导入后直连（MobaXterm）
    UnsupportedProxy,
    /// 这台主机是为来源中的会话 `session` 的 SSH 网关新建的（MobaXterm）
    GatewayFor { session: String },
    /// 来源中的 `count` 条端口转发没有导入（Termora）
    ForwardsNotImported { count: usize },
    /// 文件中开启了 agent 转发，导入后关闭（服务器可以借用本机的 agent 登录别的服务器，需要时自己打开；主机文件、
    /// Xshell、MobaXterm）
    AgentForwardDisabled,
    /// 来源中的终端编码 `name` 不支持，导入后按 UTF-8
    UnsupportedEncoding { name: String },
    /// 来源中使用代理，代理的设置保存在来源自己的代理列表中，没有导入，导入后直连；`name` 是它在来源中的名称
    /// （Xshell；FinalShell 的连接中只有代理的 ID，为 `None`）
    ProxyNotImported { name: Option<String> },
    /// 来源中设置了跳板机，格式无法确定，没有导入，导入后直连（WindTerm）
    JumpNotImported,
    /// 来源中的登录脚本（等待提示、自动发送）没有导入（Xshell）
    LoginScriptNotImported,
    /// 串口参数是按来源保存的编号换算的，导入后请核对；无法换算的流控按不流控导入（Xshell、WindTerm）
    SerialSettingsUnverified,
    /// 来源中保存的密码没能解开（可能是用旧的主密码保存的，或数据来自另一台电脑、另一个用户），导入后连接时输入
    /// （WindTerm、NyaTerm）
    SavedPasswordUnreadable,
    /// 来源导出的文件中这台主机的密码是密文（导出时没能解开），没有导入，连接时输入（Netcatty）
    SavedPasswordEncrypted,
    /// 来源中保存的密码格式不对，没能解开，没有导入，连接时输入（FinalShell）
    SavedPasswordInvalid,
    /// 来源中的验证码没有导入（不是 TOTP、参数 Termlet 不支持，或密钥没能解开），需要时在主机设置中填写（NyaTerm）
    TotpNotImported,
    /// 来源中用凭据档案登录，文件中没有这个档案，导入后需要设置登录方式（Electerm 的书签导出不含档案）
    CredentialProfileMissing,
    /// 来源中“无密码”登录（依次试 SSH Agent 与 `~/.ssh` 的默认私钥），导入为 ssh-agent：用默认私钥文件登录的
    /// 要改为私钥认证（ShellRS）
    NoPasswordLogin,
    /// 凭据的私钥文件加了私钥密码，私钥密码没有导入、连接时输入；文件在来源的数据目录中，在来源中删除凭据会一并
    /// 删除它（ShellRS）
    EncryptedKeyFile,
    /// 来源中开了验证码自动填写，密钥没有导入（在系统钥匙串里，或同步数据中没有），需要时在主机设置中填写（ShellRS）
    TotpSecretMissing,
    /// 来源中自定义的验证码识别文本没有导入，Termlet 按常见的说法识别（ShellRS）
    TotpPromptIgnored,
    /// 来源中 Telnet 自定义的登录、密码提示没有导入，Termlet 按常见的提示自动登录（ShellRS）
    TelnetPromptsIgnored,
    /// 本地、动态转发监听网卡地址 `from`：Termlet 只允许回环地址或任意地址，导入时改为 127.0.0.1（ShellRS）
    ForwardBindLoopback { from: String },
    /// 这台主机是为来源中的主机 `session` 的跳板机新建的（Electerm 的跳板机写在书签里，不是另一个书签）
    JumpHostFor { session: String },
}

/// 按名称（不分大小写）对应 Termlet 支持的编码：UTF-8、GBK / GB2312 / GB18030、Big5。
pub(super) fn encoding_by_name(name: &str) -> Option<Encoding> {
    let name = name.trim().to_ascii_lowercase().replace(['-', '_'], "");
    match name.as_str() {
        "utf8" => Some(Encoding::Utf8),
        "gbk" | "gb2312" | "gb18030" | "cp936" | "euccn" => Some(Encoding::Gb18030),
        "big5" | "cp950" | "big5hkscs" => Some(Encoding::Big5),
        _ => None,
    }
}

/// Windows 代码页对应的编码：65001 UTF-8，936 / 54936 / 20936 GB18030，950 Big5。
pub(super) fn encoding_by_code_page(page: u32) -> Option<Encoding> {
    match page {
        65001 => Some(Encoding::Utf8),
        936 | 54936 | 20936 => Some(Encoding::Gb18030),
        950 => Some(Encoding::Big5),
        _ => None,
    }
}

/// 逐台加入主机，最后统一建立跳板引用。
pub(super) struct PlanBuilder {
    plan: ImportPlan,
    groups: HashMap<(Option<usize>, String), usize>,
    keys: HashSet<String>,
}

impl PlanBuilder {
    pub fn new(source: Source) -> Self {
        Self {
            plan: ImportPlan::new(source),
            groups: HashMap::new(),
            keys: HashSet::new(),
        }
    }

    /// 按各级名称找到或新建分组，返回最后一级；名称去掉首尾空白，空名称跳过；没有名称时为根（`None`）。
    pub fn group<S: AsRef<str>>(&mut self, path: &[S]) -> Option<usize> {
        let mut parent = None;
        for name in path {
            let name = name.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            let groups = &mut self.plan.groups;
            let index = *self
                .groups
                .entry((parent, name.to_string()))
                .or_insert_with(|| {
                    groups.push(PlannedGroup {
                        name: name.to_string(),
                        parent,
                    });
                    groups.len() - 1
                });
            parent = Some(index);
        }
        parent
    }

    /// 加入一台主机，返回它的下标。键重复时追加序号（`#2`、`#3`……）。
    pub fn push(&mut self, mut host: PlannedHost) -> usize {
        host.key = self.unique_key(&host.key);
        self.plan.hosts.push(host);
        self.plan.hosts.len() - 1
    }

    fn unique_key(&mut self, key: &str) -> String {
        let mut candidate = key.to_string();
        let mut n = 2;
        while self.keys.contains(&candidate) {
            candidate = format!("{key}#{n}");
            n += 1;
        }
        self.keys.insert(candidate.clone());
        candidate
    }

    pub fn add_key(&mut self, key: PlannedKey) -> usize {
        self.plan.keys.push(key);
        self.plan.keys.len() - 1
    }

    /// 加入一个凭据档案，返回它的下标（主机用 [`PlannedHost::set_profile`] 引用）。
    pub fn add_profile(&mut self, profile: PlannedProfile) -> usize {
        self.plan.profiles.push(profile);
        self.plan.profiles.len() - 1
    }

    /// 加入一条整个计划的说明（相同的只记一次）。
    pub fn note(&mut self, note: PlanNote) {
        if !self.plan.notes.contains(&note) {
            self.plan.notes.push(note);
        }
    }

    pub fn skip(&mut self, n: usize) {
        self.plan.skipped += n;
    }

    pub fn add_snippet(&mut self, snippet: Snippet) {
        self.plan.snippets.push(snippet);
    }

    pub fn snippet_notes_mut(&mut self) -> &mut SnippetNotes {
        &mut self.plan.snippet_notes
    }

    pub fn hosts(&self) -> &[PlannedHost] {
        &self.plan.hosts
    }

    pub fn host_mut(&mut self, index: usize) -> &mut PlannedHost {
        &mut self.plan.hosts[index]
    }

    /// 完成：跳板机按地址、端口、用户名对应到计划中的第一台相同主机。
    pub fn finish(mut self) -> ImportPlan {
        let mut index: HashMap<Endpoint, usize> = HashMap::new();
        for (i, h) in self.plan.hosts.iter().enumerate() {
            index.entry(h.endpoint()).or_insert(i);
        }
        for (i, h) in self.plan.hosts.iter_mut().enumerate() {
            if let Some(jump) = &mut h.jump {
                jump.host = index.get(&jump.endpoint).copied().filter(|&t| t != i);
            }
        }
        self.plan
    }
}

/// 截断到 `n` 个字符，避免超出长度限制导致整台主机导入失败。
pub(super) fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((at, _)) => s[..at].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(address: &str, user: &str) -> Host {
        Host {
            name: address.into(),
            address: address.into(),
            port: 22,
            username: user.into(),
            ..Default::default()
        }
    }

    #[test]
    fn groups_are_shared_and_parents_come_first() {
        let mut b = PlanBuilder::new(Source::Xshell);
        let db = b.group(&["生产", " 数据库 "]);
        let web = b.group(&["生产", "", "Web"]);
        assert_eq!(b.group::<&str>(&[]), None);
        assert_eq!(b.group(&[" "]), None);
        assert_eq!(b.group(&["生产", "数据库"]), db);
        let plan = b.finish();
        assert_eq!(plan.groups.len(), 3);
        assert_eq!(plan.group_path(db.unwrap()), vec!["生产", "数据库"]);
        assert_eq!(plan.group_path(web.unwrap()), vec!["生产", "Web"]);
        for (i, g) in plan.groups.iter().enumerate() {
            assert!(g.parent.is_none_or(|p| p < i));
        }
    }

    #[test]
    fn keys_are_made_unique() {
        let mut b = PlanBuilder::new(Source::Tabby);
        for _ in 0..3 {
            b.push(PlannedHost::new("a", host("h", "u")));
        }
        let keys: Vec<_> = b.hosts().iter().map(|h| h.key.clone()).collect();
        assert_eq!(keys, ["a", "a#2", "a#3"]);
    }

    #[test]
    fn jumps_link_to_the_first_matching_host_but_not_to_itself() {
        let mut b = PlanBuilder::new(Source::SshConfig);
        b.push(PlannedHost::new("bastion", host("Bastion.Example", "jump")));
        b.push(PlannedHost::new(
            "bastion-2",
            host("bastion.example", "jump"),
        ));
        let mut app = PlannedHost::new("app", host("10.0.0.1", "deploy"));
        app.jump = Some(JumpRef::new(
            "bastion",
            Endpoint::new("BASTION.example", 0, "jump"),
        ));
        b.push(app);
        let mut own = PlannedHost::new("self", host("10.0.0.2", "u"));
        own.jump = Some(JumpRef::new("self", Endpoint::new("10.0.0.2", 22, "u")));
        b.push(own);
        let mut outside = PlannedHost::new("outside", host("10.0.0.3", "u"));
        outside.jump = Some(JumpRef::new("gw", Endpoint::new("gw", 22, "u")));
        b.push(outside);
        let plan = b.finish();
        assert_eq!(plan.hosts[2].jump.as_ref().unwrap().host, Some(0));
        assert_eq!(plan.hosts[3].jump.as_ref().unwrap().host, None);
        assert_eq!(plan.hosts[4].jump.as_ref().unwrap().host, None);
    }

    #[test]
    fn credentials_are_hidden_from_debug_and_resolved_on_write() {
        let mut h = PlannedHost::new("h", host("10.0.0.1", "root"));
        h.set_password(SecretString::new("hunter2"));
        h.set_proxy_password(SecretString::new(""));
        h.set_key(0);
        assert!(h.has_password() && !h.has_proxy_password());
        assert_eq!(h.password().unwrap().expose(), "hunter2");
        assert!(!format!("{h:?}").contains("hunter2"));

        let key = KeyId::from_raw("k_1");
        let jump = HostId::from_raw("h_1");
        let host = h.to_host(Some(key.clone()), Some(jump.clone()), None);
        assert_eq!(
            host.auth,
            Auth::Key {
                source: KeySource::Library { key }
            }
        );
        assert_eq!(host.jump, Jump::Via { host: jump });
        // 私钥没有存进密钥库时保持原来的认证方式
        assert_eq!(h.to_host(None, None, None).auth, Auth::Password);
    }

    #[test]
    fn clip_counts_characters() {
        assert_eq!(clip("长长长", 2), "长长");
        assert_eq!(clip("ab", 5), "ab");
    }
}
