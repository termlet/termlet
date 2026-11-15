//! 分组默认值的沿用：分组可以设默认的用户名、凭据档案、跳板机与代理命令，分组及其子分组中没有设置这些项的主机
//! 逐级向上沿用。主机对话框、分组对话框据此显示"沿用来的值与来源"；连接时的解析见 `ssh::plan`。

use crate::id::{GroupId, HostId, ProfileId};
use crate::model::Group;
use crate::store::{Record, Store};

/// 沿用来的一个值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inherit<T> {
    pub value: T,
    /// 设置这个值的分组
    pub from: GroupId,
    pub group_name: String,
}

/// 从某个分组开始向上能沿用到的默认值。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inherited {
    pub username: Option<Inherit<String>>,
    pub profile: Option<Inherit<ProfileId>>,
    pub jump: Option<Inherit<HostId>>,
    pub proxy_command: Option<Inherit<String>>,
}

/// 放在 `group` 中的主机（或 `group` 的子分组）能沿用到的默认值：从 `group` 自己开始逐级向上，取最近的。
pub fn inherited(store: &Store, group: Option<&GroupId>) -> Inherited {
    from_chain(&store.group_chain(group))
}

fn from_chain(chain: &[Record<Group>]) -> Inherited {
    /// 最近的一个设置了这一项的分组。
    fn nearest<T>(
        chain: &[Record<Group>],
        value: impl Fn(&Group) -> Option<T>,
    ) -> Option<Inherit<T>> {
        chain.iter().find_map(|g| {
            value(&g.doc).map(|value| Inherit {
                value,
                from: g.id.clone(),
                group_name: g.doc.name.clone(),
            })
        })
    }
    Inherited {
        username: nearest(chain, |g| {
            Some(g.defaults.username.trim().to_string()).filter(|u| !u.is_empty())
        }),
        profile: nearest(chain, |g| g.defaults.profile.clone()),
        jump: nearest(chain, |g| g.defaults.jump.clone()),
        proxy_command: nearest(chain, |g| {
            Some(g.defaults.proxy_command.trim().to_string()).filter(|c| !c.is_empty())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GroupDefaults;
    use crate::store::Position;
    use crate::testkit::memory_store;

    fn group(store: &Store, name: &str, parent: Option<&GroupId>, username: &str) -> GroupId {
        store
            .create(
                Group {
                    name: name.into(),
                    defaults: GroupDefaults {
                        username: username.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                parent.map(|p| p.as_str().to_string()),
                Position::Last,
            )
            .unwrap()
    }

    #[test]
    fn nearest_group_wins_and_blank_values_are_skipped() {
        let store = memory_store();
        let top = group(&store, "生产", None, "deploy");
        let mid = group(&store, "华东", Some(&top), "  ");
        let leaf = group(&store, "web", Some(&mid), "");
        let inherited = inherited(&store, Some(&leaf));
        let username = inherited.username.unwrap();
        assert_eq!(username.value, "deploy");
        assert_eq!(username.from, top);
        assert_eq!(username.group_name, "生产");
        assert!(inherited.profile.is_none() && inherited.jump.is_none());

        let own = group(&store, "db", Some(&mid), "dba");
        assert_eq!(
            super::inherited(&store, Some(&own)).username.unwrap().value,
            "dba"
        );
        assert_eq!(
            super::inherited(&store, None),
            Inherited::default(),
            "未分组没有可沿用的"
        );
    }
}
