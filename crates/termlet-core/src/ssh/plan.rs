//! 连接计划：从存储中解析出连接一台主机要经过的每一跳（跳板机在前，目标在最后），
//! 以及每一跳的地址、用户名、认证材料、代理与算法选项。
//!
//! 解析规则：
//! - 用户名：主机自己的 → 凭据档案的 → 分组及其上级的默认值 → 本机的用户名。
//! - 认证："沿用分组"时取最近的分组默认凭据档案，没有时同"密码"。
//! - 跳板机："沿用分组"时取最近的分组默认跳板机；跳板机自己的跳板机继续向上，最多 5 层，不能循环。
//! - 代理：主机自己的；没有时沿用最近的分组默认代理命令（主机选了"不使用代理"时不沿用）。
//!   代理只对第一跳生效：经跳板机连接的主机不使用自己的代理（跳板机自己的代理照常生效）。

use std::path::PathBuf;

use crate::id::{GroupId, HostId};
use crate::model::{
    Auth, CliAuth, Group, Host, Jump, KeySource, Kind, MAX_JUMP_DEPTH, Profile, ProfileAuth,
    ProxyKind, SecretField,
};
use crate::quick::QuickTarget;
use crate::secret::SecretString;
use crate::ssh::SshError;
use crate::store::{Owner, Record, Store};

/// 私钥从哪里读。
#[derive(Clone, Debug)]
pub enum KeyMaterial {
    /// 私钥文本（密钥库中的、粘贴的）
    Text(SecretString),
    /// 本机的私钥文件
    File(PathBuf),
}

/// 与私钥一起出示的 OpenSSH 证书从哪里来（见 [`crate::sshcert`]）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CertMaterial {
    /// 私钥文件旁同名的 `-cert`、`-cert.pub`（与 OpenSSH 相同）；没有时不出示
    Beside(PathBuf),
    /// 指定的证书文件（OpenSSH 的 `CertificateFile`）
    File(PathBuf),
    /// 密钥库中的私钥、粘贴的私钥附带的证书（一行 OpenSSH 证书）
    Text(String),
}

/// 一跳的认证方式。
#[derive(Clone, Debug)]
pub enum AuthPlan {
    /// 密码；没有保存时连接中询问
    Password { saved: Option<SecretString> },
    /// 私钥；加密的私钥用保存的私钥密码，没有时询问。有证书时先出示证书
    Key {
        key: KeyMaterial,
        passphrase: Option<SecretString>,
        /// 显示用的名称（密钥库中的名称或文件名）
        label: String,
        certificate: Option<CertMaterial>,
    },
    /// ssh-agent / Pageant
    Agent,
    /// 快速连接：先试 agent，再询问密码
    AgentThenPassword,
}

/// 代理。
#[derive(Clone, Debug)]
pub struct ProxyPlan {
    pub kind: ProxyKind,
    pub address: String,
    pub port: u16,
    pub username: String,
    pub password: Option<SecretString>,
    pub command: String,
    /// 代理命令中的 `%n`：主机的名称（OpenSSH 中是命令行上写的主机别名；从 ssh_config 导入的主机，名称就是别名）
    pub alias: String,
    /// 代理命令沿用自哪个分组（显示用）；主机自己的为 `None`
    pub from_group: Option<String>,
}

/// 连接中的一跳。
#[derive(Clone, Debug)]
pub struct Hop {
    /// 显示用的名称（询问密码、报错时）
    pub label: String,
    pub host_id: Option<HostId>,
    pub address: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthPlan,
    /// TOTP 密钥：键盘交互认证问验证码时自动回答
    pub totp: Option<SecretString>,
    pub proxy: Option<ProxyPlan>,
    /// 兼容旧设备
    pub legacy_algorithms: bool,
}

impl Hop {
    /// 保存的登录密码（认证方式为密码且保存了时）；终端中出现 sudo 密码提示时可以填入。
    pub fn saved_password(&self) -> Option<&SecretString> {
        match &self.auth {
            AuthPlan::Password { saved } => saved.as_ref(),
            _ => None,
        }
    }
}

/// 连接一台主机的计划。
#[derive(Clone, Debug)]
pub struct ConnectPlan {
    /// 跳板机在前，目标在最后
    pub hops: Vec<Hop>,
}

impl ConnectPlan {
    pub fn target(&self) -> &Hop {
        self.hops.last().expect("连接计划至少有一跳")
    }
}

/// 快速连接（也用于被堡垒机等调起）的计划：临时主机，没有保存的凭据。
/// 兼容旧算法（老堡垒机、交换机常见；安全的算法仍排在前面）。
pub fn plan_for_quick(target: &QuickTarget) -> ConnectPlan {
    ConnectPlan {
        hops: vec![Hop {
            // 询问密码时与用户名拼在一起显示，这里不带用户名
            label: target.host_display(),
            host_id: None,
            address: target.host.clone(),
            port: target.port,
            username: target.username.clone(),
            auth: AuthPlan::AgentThenPassword,
            totp: None,
            proxy: None,
            legacy_algorithms: true,
        }],
    }
}

/// 一台保存的主机的计划。
pub fn plan_for_host(store: &Store, host: &HostId) -> Result<ConnectPlan, SshError> {
    let mut visited = Vec::new();
    let hops = resolve_chain(store, host, &mut visited)?;
    Ok(finish(hops))
}

/// 命令行工具（`termlet`）连接一台主机的计划：
/// - 跳板机照常，用各自的凭据（与 TOTP）；
/// - 目标主机设置了 CLI 专用凭据时用它登录（用户名留空时用主机实际使用的用户名），否则用主机自己的凭据；
/// - 目标主机不使用 TOTP：否则命令行能在无人值守时登录需要验证码的主机，双重验证形同虚设。
pub fn plan_for_cli(store: &Store, host: &HostId) -> Result<ConnectPlan, SshError> {
    let mut plan = plan_for_host(store, host)?;
    let record = store
        .get::<Host>(host)
        .ok_or_else(|| SshError::Config("主机已删除".into()))?;
    let owner = Owner::host(host);
    let target = plan.hops.last_mut().expect("连接计划至少有一跳");
    target.totp = None;
    if let Some(credential) = &record.doc.cli.credential {
        if !credential.username.trim().is_empty() {
            target.username = credential.username.clone();
        }
        // 专用凭据的秘密存在主机的 cli_* 字段中
        let cli_secret = |field: SecretField| {
            let field = match field {
                SecretField::Password => SecretField::CliPassword,
                SecretField::PrivateKey => SecretField::CliPrivateKey,
                SecretField::Passphrase => SecretField::CliPassphrase,
                other => other,
            };
            read_secret(store, &owner, field)
        };
        target.auth = match &credential.auth {
            CliAuth::Password => AuthPlan::Password {
                saved: cli_secret(SecretField::Password)?,
            },
            CliAuth::Key { source } => key_plan(store, source, &cli_secret)?,
            CliAuth::Agent => AuthPlan::Agent,
        };
    }
    Ok(plan)
}

/// 主机对话框中尚未保存的主机（测试连接）：用填写的内容，秘密取对话框中的修改，没改的取已保存的。
pub fn plan_for_draft(store: &Store, draft: &HostDraft<'_>) -> Result<ConnectPlan, SshError> {
    let mut visited: Vec<HostId> = draft.id.cloned().into_iter().collect();
    let chain = store.group_chain(draft.group);
    let mut hops = match jump_of(draft.host, draft.id, &chain) {
        Some(jump) => resolve_chain(store, &jump, &mut visited)?,
        None => Vec::new(),
    };
    let own = |field: SecretField| -> Result<Option<SecretString>, SshError> {
        match draft.secrets.get(field) {
            SecretEdit::Set(value) => Ok(Some(value.clone())),
            SecretEdit::Clear => Ok(None),
            SecretEdit::Keep => match draft.id {
                Some(id) => read_secret(store, &Owner::host(id), field),
                None => Ok(None),
            },
        }
    };
    hops.push(resolve_hop(store, draft.id, draft.host, &chain, &own)?);
    Ok(finish(hops))
}

/// 对话框中填写的主机。
pub struct HostDraft<'a> {
    /// 编辑已有的主机时是它的 ID（没改的秘密取已保存的）
    pub id: Option<&'a HostId>,
    pub group: Option<&'a GroupId>,
    pub host: &'a Host,
    pub secrets: &'a SecretEdits,
}

/// 一项秘密怎么改。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SecretEdit {
    /// 不改（留空保持不变）
    #[default]
    Keep,
    Set(SecretString),
    /// 删除保存的
    Clear,
}

/// 对话框中对主机秘密的修改。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecretEdits {
    pub password: SecretEdit,
    pub private_key: SecretEdit,
    pub passphrase: SecretEdit,
    pub totp: SecretEdit,
    pub proxy_password: SecretEdit,
    /// 命令行工具专用的凭据
    pub cli_password: SecretEdit,
    pub cli_private_key: SecretEdit,
    pub cli_passphrase: SecretEdit,
}

impl SecretEdits {
    pub fn get(&self, field: SecretField) -> &SecretEdit {
        match field {
            SecretField::Password => &self.password,
            SecretField::PrivateKey => &self.private_key,
            SecretField::Passphrase => &self.passphrase,
            SecretField::Totp => &self.totp,
            SecretField::ProxyPassword => &self.proxy_password,
            SecretField::CliPassword => &self.cli_password,
            SecretField::CliPrivateKey => &self.cli_private_key,
            SecretField::CliPassphrase => &self.cli_passphrase,
        }
    }

    /// 写入存储（保存主机之后）。
    pub fn apply(&self, store: &Store, owner: &Owner) -> crate::Result<()> {
        for (field, edit) in [
            (SecretField::Password, &self.password),
            (SecretField::PrivateKey, &self.private_key),
            (SecretField::Passphrase, &self.passphrase),
            (SecretField::Totp, &self.totp),
            (SecretField::ProxyPassword, &self.proxy_password),
            (SecretField::CliPassword, &self.cli_password),
            (SecretField::CliPrivateKey, &self.cli_private_key),
            (SecretField::CliPassphrase, &self.cli_passphrase),
        ] {
            match edit {
                SecretEdit::Keep => {}
                SecretEdit::Set(value) => store.set_secret(owner, field, Some(value))?,
                SecretEdit::Clear => store.set_secret(owner, field, None)?,
            }
        }
        Ok(())
    }
}

/// 只有第一跳的代理生效。
fn finish(mut hops: Vec<Hop>) -> ConnectPlan {
    for hop in hops.iter_mut().skip(1) {
        hop.proxy = None;
    }
    ConnectPlan { hops }
}

/// 主机的跳板机："沿用分组"时取最近的分组默认值；分组的默认跳板机就是这台主机自己时忽略（直连）。
fn jump_of(host: &Host, id: Option<&HostId>, chain: &[Record<Group>]) -> Option<HostId> {
    match &host.jump {
        Jump::Direct => None,
        Jump::Via { host } => Some(host.clone()),
        Jump::Inherit => chain
            .iter()
            .find_map(|g| g.doc.defaults.jump.clone())
            .filter(|jump| Some(jump) != id),
    }
}

fn read_secret(
    store: &Store,
    owner: &Owner,
    field: SecretField,
) -> Result<Option<SecretString>, SshError> {
    store
        .secret(owner, field)
        .map_err(|e| SshError::Vault(e.to_string()))
}

fn resolve_chain(
    store: &Store,
    host_id: &HostId,
    visited: &mut Vec<HostId>,
) -> Result<Vec<Hop>, SshError> {
    if visited.contains(host_id) {
        return Err(SshError::Config("跳板机形成了循环".into()));
    }
    if visited.len() > MAX_JUMP_DEPTH {
        return Err(SshError::Config(format!("跳板机超过 {MAX_JUMP_DEPTH} 层")));
    }
    visited.push(host_id.clone());
    let record = store
        .get::<Host>(host_id)
        .ok_or_else(|| SshError::Config("引用的主机已删除".into()))?;
    let group = record.parent.clone().map(GroupId::from_raw);
    let chain = store.group_chain(group.as_ref());
    let mut hops = match jump_of(&record.doc, Some(host_id), &chain) {
        Some(jump) => resolve_chain(store, &jump, visited)?,
        None => Vec::new(),
    };
    let owner = Owner::host(host_id);
    let own = |field: SecretField| read_secret(store, &owner, field);
    hops.push(resolve_hop(
        store,
        Some(host_id),
        &record.doc,
        &chain,
        &own,
    )?);
    Ok(hops)
}

/// 主机自己的秘密（保存的，或对话框中修改的）。
pub(crate) type OwnSecrets<'a> = dyn Fn(SecretField) -> Result<Option<SecretString>, SshError> + 'a;

fn resolve_hop(
    store: &Store,
    host_id: Option<&HostId>,
    host: &Host,
    chain: &[Record<Group>],
    own: &OwnSecrets<'_>,
) -> Result<Hop, SshError> {
    // 凭据档案：主机指定的，或"沿用分组"时最近的分组默认档案
    let profile_id = match &host.auth {
        Auth::Profile { profile } => Some(profile.clone()),
        Auth::Inherit => chain.iter().find_map(|g| g.doc.defaults.profile.clone()),
        _ => None,
    };
    let profile = profile_id
        .as_ref()
        .map(|id| {
            store
                .get::<Profile>(id)
                .ok_or_else(|| SshError::Config("引用的凭据档案已删除".into()))
                .map(|record| (id.clone(), record.doc))
        })
        .transpose()?;

    let username = [
        Some(host.username.clone()),
        profile.as_ref().map(|(_, p)| p.username.clone()),
    ]
    .into_iter()
    .flatten()
    .chain(chain.iter().map(|g| g.doc.defaults.username.clone()))
    .find(|u| !u.trim().is_empty())
    .unwrap_or_else(local_username);

    let auth = match (&host.auth, &profile) {
        (_, Some((profile_id, profile))) => {
            let owner = Owner::profile(profile_id);
            let profile_secret = |field: SecretField| read_secret(store, &owner, field);
            match &profile.auth {
                ProfileAuth::Password => AuthPlan::Password {
                    saved: profile_secret(SecretField::Password)?,
                },
                ProfileAuth::Key { source } => key_plan(store, source, &profile_secret)?,
                ProfileAuth::Agent => AuthPlan::Agent,
            }
        }
        (Auth::Key { source }, None) => key_plan(store, source, own)?,
        (Auth::Agent, None) => AuthPlan::Agent,
        // 密码，或"沿用分组"而分组没有默认档案
        _ => AuthPlan::Password {
            saved: own(SecretField::Password)?,
        },
    };

    let proxy = proxy_plan(host, chain, own)?;

    Ok(Hop {
        label: host.name.clone(),
        host_id: host_id.cloned(),
        address: host.address.clone(),
        port: host.effective_port(),
        username,
        auth,
        totp: own(SecretField::Totp)?,
        proxy,
        legacy_algorithms: host.ssh.legacy_algorithms,
    })
}

/// 主机的代理：自己的；没有时沿用最近的分组默认代理命令（选了"不使用代理"时不沿用）。
/// 是否经跳板机（经跳板机时不用）由调用方决定。
pub(crate) fn proxy_plan(
    host: &Host,
    chain: &[Record<Group>],
    own: &OwnSecrets<'_>,
) -> Result<Option<ProxyPlan>, SshError> {
    if let Some(proxy) = &host.proxy {
        return Ok(Some(ProxyPlan {
            kind: proxy.kind,
            address: proxy.address.clone(),
            port: proxy.port,
            username: proxy.username.clone(),
            password: own(SecretField::ProxyPassword)?,
            command: proxy.command.clone(),
            alias: host.name.clone(),
            from_group: None,
        }));
    }
    if host.no_proxy {
        return Ok(None);
    }
    Ok(chain.iter().find_map(|g| {
        let command = g.doc.defaults.proxy_command.trim();
        (!command.is_empty()).then(|| ProxyPlan {
            kind: ProxyKind::Command,
            address: String::new(),
            port: 0,
            username: String::new(),
            password: None,
            command: command.to_string(),
            alias: host.name.clone(),
            from_group: Some(g.doc.name.clone()),
        })
    }))
}

/// 私钥认证的材料。`own` 读取主机或凭据档案自己的秘密（粘贴的私钥、私钥密码）。
fn key_plan(store: &Store, source: &KeySource, own: &OwnSecrets<'_>) -> Result<AuthPlan, SshError> {
    Ok(match source {
        KeySource::Library { key } => {
            let record = store
                .get::<crate::model::KeyEntry>(key)
                .ok_or_else(|| SshError::Config("引用的密钥已从密钥库删除".into()))?;
            let text = read_secret(
                store,
                &Owner::new(Kind::Key, key.as_str()),
                SecretField::PrivateKey,
            )?
            .ok_or_else(|| SshError::Key("密钥库中没有这把私钥".into()))?;
            let certificate = record.doc.certificate.trim();
            AuthPlan::Key {
                key: KeyMaterial::Text(text),
                passphrase: None,
                certificate: (!certificate.is_empty())
                    .then(|| CertMaterial::Text(certificate.to_string())),
                label: record.doc.name,
            }
        }
        KeySource::File { path, certificate } => AuthPlan::Key {
            key: KeyMaterial::File(PathBuf::from(path)),
            passphrase: own(SecretField::Passphrase)?,
            label: std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone()),
            certificate: Some(match certificate.trim() {
                "" => CertMaterial::Beside(PathBuf::from(path)),
                file => CertMaterial::File(PathBuf::from(file)),
            }),
        },
        KeySource::Pasted { certificate } => AuthPlan::Key {
            key: KeyMaterial::Text(
                own(SecretField::PrivateKey)?
                    .ok_or_else(|| SshError::Key("没有保存粘贴的私钥".into()))?,
            ),
            passphrase: own(SecretField::Passphrase)?,
            label: String::new(),
            certificate: Some(certificate.trim())
                .filter(|c| !c.is_empty())
                .map(|c| CertMaterial::Text(c.to_string())),
        },
    })
}

/// 本机的用户名（与 OpenSSH 相同：没有指定用户名时用它）。
fn local_username() -> String {
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).unwrap_or_else(|_| "root".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Position;
    use crate::testkit::memory_store;

    fn host(name: &str) -> Host {
        Host {
            name: name.into(),
            address: format!("{name}.example"),
            username: "ops".into(),
            ..Default::default()
        }
    }

    fn saved_password(plan: &ConnectPlan) -> Option<String> {
        match &plan.target().auth {
            AuthPlan::Password { saved } => saved.as_ref().map(|s| s.expose().to_string()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn draft_uses_edited_secrets_and_falls_back_to_saved_ones() {
        let store = memory_store();
        let bastion = store.create(host("bastion"), None, Position::Last).unwrap();
        let id = store.create(host("web"), None, Position::Last).unwrap();
        store
            .set_secret(
                &Owner::host(&id),
                SecretField::Password,
                Some(&SecretString::new("saved")),
            )
            .unwrap();

        // 新建：没有已保存的，用对话框里输入的
        let mut doc = host("new");
        doc.jump = Jump::Via {
            host: bastion.clone(),
        };
        let mut secrets = SecretEdits {
            password: SecretEdit::Set(SecretString::new("typed")),
            ..Default::default()
        };
        let draft = HostDraft {
            id: None,
            group: None,
            host: &doc,
            secrets: &secrets,
        };
        let plan = plan_for_draft(&store, &draft).unwrap();
        assert_eq!(plan.hops.len(), 2, "跳板机在前");
        assert_eq!(plan.hops[0].host_id.as_ref(), Some(&bastion));
        assert_eq!(plan.target().address, "new.example");
        assert_eq!(saved_password(&plan).as_deref(), Some("typed"));

        // 编辑：留空保持不变时用已保存的，清除时不用
        let doc = store.get::<Host>(&id).unwrap().doc;
        secrets.password = SecretEdit::Keep;
        let draft = HostDraft {
            id: Some(&id),
            group: None,
            host: &doc,
            secrets: &secrets,
        };
        assert_eq!(
            saved_password(&plan_for_draft(&store, &draft).unwrap()).as_deref(),
            Some("saved")
        );
        secrets.password = SecretEdit::Clear;
        let draft = HostDraft {
            id: Some(&id),
            group: None,
            host: &doc,
            secrets: &secrets,
        };
        assert_eq!(
            saved_password(&plan_for_draft(&store, &draft).unwrap()),
            None
        );

        // 跳板机指向正在编辑的这台自己：循环
        let mut looped = doc.clone();
        looped.jump = Jump::Via { host: id.clone() };
        let draft = HostDraft {
            id: Some(&id),
            group: None,
            host: &looped,
            secrets: &secrets,
        };
        assert!(plan_for_draft(&store, &draft).is_err());
    }

    #[test]
    fn group_default_jump_is_ignored_for_the_jump_host_itself() {
        use crate::model::GroupDefaults;
        let store = memory_store();
        let group = store
            .create(
                Group {
                    name: "g".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let bastion = store
            .create(
                host("bastion"),
                Some(group.as_str().to_string()),
                Position::Last,
            )
            .unwrap();
        let web = store
            .create(
                host("web"),
                Some(group.as_str().to_string()),
                Position::Last,
            )
            .unwrap();
        let mut doc = store.get::<Group>(&group).unwrap().doc;
        doc.defaults = GroupDefaults {
            jump: Some(bastion.clone()),
            ..Default::default()
        };
        store.update(&group, doc).unwrap();
        // 组里的其他主机经跳板机，跳板机自己直连
        assert_eq!(plan_for_host(&store, &web).unwrap().hops.len(), 2);
        assert_eq!(plan_for_host(&store, &bastion).unwrap().hops.len(), 1);
    }

    // 分组的代理命令：没有自己代理的主机沿用（%n 是主机的名称），选了"不使用代理"的不沿用，自己的优先；
    // 经跳板机时目标不用代理，跳板机自己沿用照常
    #[test]
    fn group_proxy_command_is_inherited() {
        use crate::model::{GroupDefaults, Proxy};
        let store = memory_store();
        let group = store
            .create(
                Group {
                    name: "AWS".into(),
                    defaults: GroupDefaults {
                        proxy_command: "aws ssm start-session --target %n".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let parent = Some(group.as_str().to_string());
        let inherits = store
            .create(host("i-01"), parent.clone(), Position::Last)
            .unwrap();
        let mut direct = host("direct");
        direct.no_proxy = true;
        let direct = store
            .create(direct, parent.clone(), Position::Last)
            .unwrap();
        let mut own = host("own");
        own.proxy = Some(Proxy {
            kind: ProxyKind::Command,
            command: "nc %h %p".into(),
            ..Default::default()
        });
        let own = store.create(own, parent.clone(), Position::Last).unwrap();
        let mut behind = host("behind");
        behind.jump = Jump::Via {
            host: inherits.clone(),
        };
        let behind = store.create(behind, parent, Position::Last).unwrap();

        let plan = plan_for_host(&store, &inherits).unwrap();
        let proxy = plan.target().proxy.as_ref().unwrap();
        assert_eq!(proxy.kind, ProxyKind::Command);
        assert_eq!(proxy.command, "aws ssm start-session --target %n");
        assert_eq!(proxy.alias, "i-01");
        assert_eq!(proxy.from_group.as_deref(), Some("AWS"));
        assert!(
            plan_for_host(&store, &direct)
                .unwrap()
                .target()
                .proxy
                .is_none()
        );
        let plan = plan_for_host(&store, &own).unwrap();
        let proxy = plan.target().proxy.as_ref().unwrap();
        assert_eq!(
            (proxy.command.as_str(), proxy.from_group.as_ref()),
            ("nc %h %p", None)
        );
        let plan = plan_for_host(&store, &behind).unwrap();
        assert_eq!(plan.hops.len(), 2);
        assert!(plan.hops[0].proxy.is_some(), "跳板机沿用分组的代理命令");
        assert!(plan.target().proxy.is_none(), "经跳板机的目标不用代理");
        // Telnet 主机同样沿用
        let mut telnet = host("switch");
        telnet.protocol = crate::model::Protocol::Telnet;
        let telnet = store
            .create(telnet, Some(group.as_str().to_string()), Position::Last)
            .unwrap();
        let plan = crate::telnet::plan_for_host(&store, &telnet).unwrap();
        assert_eq!(plan.proxy.unwrap().alias, "switch");
    }

    #[test]
    fn secret_edits_apply_to_store() {
        let store = memory_store();
        let id = store.create(host("web"), None, Position::Last).unwrap();
        let owner = Owner::host(&id);
        store
            .set_secret(
                &owner,
                SecretField::Totp,
                Some(&SecretString::new("JBSWY3DP")),
            )
            .unwrap();
        let edits = SecretEdits {
            password: SecretEdit::Set(SecretString::new("p")),
            totp: SecretEdit::Clear,
            ..Default::default()
        };
        edits.apply(&store, &owner).unwrap();
        assert!(store.has_secret(&owner, SecretField::Password));
        assert!(!store.has_secret(&owner, SecretField::Totp));
    }
}
