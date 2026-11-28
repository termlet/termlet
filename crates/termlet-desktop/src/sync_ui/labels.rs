//! 同步界面中各类数据的名称、数量对比与冲突的说明（设置页与对话框共用）。

use gpui_kit::SharedString;
use termlet_core::cloudsync::{Conflict, RecordKind, Side, SyncCounts};

use crate::t;

/// 一条数据的种类（冲突列表的“种类”一栏）。
pub(super) fn kind_label(kind: RecordKind) -> SharedString {
    match kind {
        RecordKind::Host => t!("sync_ui.kind.host"),
        RecordKind::Group => t!("sync_ui.kind.group"),
        RecordKind::Profile => t!("sync_ui.kind.profile"),
        RecordKind::Key => t!("sync_ui.kind.key"),
        RecordKind::Snippet => t!("sync_ui.kind.snippet"),
        RecordKind::Forward => t!("sync_ui.kind.forward"),
        RecordKind::Workspace => t!("sync_ui.kind.workspace"),
        RecordKind::Scheme => t!("sync_ui.kind.scheme"),
        RecordKind::Settings => t!("sync_ui.kind.settings"),
    }
}

/// 一类数据的总称（数量对比的行名）。
pub(super) fn count_label(kind: RecordKind) -> SharedString {
    match kind {
        RecordKind::Host => t!("sync_ui.count.hosts"),
        RecordKind::Group => t!("sync_ui.count.groups"),
        RecordKind::Profile => t!("sync_ui.count.profiles"),
        RecordKind::Key => t!("sync_ui.count.keys"),
        RecordKind::Snippet => t!("sync_ui.count.snippets"),
        RecordKind::Forward => t!("sync_ui.count.forwards"),
        RecordKind::Workspace => t!("sync_ui.count.workspaces"),
        RecordKind::Scheme => t!("sync_ui.count.schemes"),
        RecordKind::Settings => t!("sync_ui.kind.settings"),
    }
}

/// 冲突中条目的名称：设置组的名称是组名（general、terminal……），换成界面上的叫法。
pub(super) fn conflict_name(conflict: &Conflict) -> SharedString {
    if conflict.kind != RecordKind::Settings {
        return conflict.name.clone().into();
    }
    match conflict.name.as_str() {
        "general" => t!("sync_ui.settings_group.general"),
        "appearance" => t!("sync_ui.settings_group.appearance"),
        "terminal" => t!("sync_ui.settings_group.terminal"),
        "highlight" => t!("sync_ui.settings_group.highlight"),
        "connection" => t!("sync_ui.settings_group.connection"),
        "guard" => t!("sync_ui.settings_group.guard"),
        "keybindings" => t!("sync_ui.settings_group.keybindings"),
        "snippets" => t!("sync_ui.settings_group.snippets"),
        other => other.to_string().into(),
    }
}

/// 冲突是怎样解决的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resolution {
    /// 两边都修改了，采用了本机的（较新）
    KeptLocal,
    /// 两边都修改了，采用了其他设备的（较新）
    KeptRemote,
    /// 其他设备删除了它，保留了本机的修改
    RemoteDeleted,
    /// 本机删除了它，保留了其他设备的修改
    LocalDeleted,
}

impl Resolution {
    pub(super) fn of(conflict: &Conflict) -> Self {
        match (conflict.winner, conflict.deleted) {
            (Side::Local, false) => Resolution::KeptLocal,
            (Side::Remote, false) => Resolution::KeptRemote,
            (Side::Local, true) => Resolution::RemoteDeleted,
            (Side::Remote, true) => Resolution::LocalDeleted,
        }
    }

    pub(super) fn text(self) -> SharedString {
        match self {
            Resolution::KeptLocal => t!("sync_ui.conflict.kept_local"),
            Resolution::KeptRemote => t!("sync_ui.conflict.kept_remote"),
            Resolution::RemoteDeleted => t!("sync_ui.conflict.remote_deleted"),
            Resolution::LocalDeleted => t!("sync_ui.conflict.local_deleted"),
        }
    }

    /// 改用落败的一边是删除它（落败的一边是删除）。
    pub(super) fn other_is_delete(self) -> bool {
        matches!(self, Resolution::RemoteDeleted | Resolution::LocalDeleted)
    }
}

/// 数量对比的一行：种类、本机的条数、同步位置的条数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CountRow {
    pub kind: RecordKind,
    pub local: usize,
    pub remote: usize,
}

/// 两边的数量对比：只列出至少一边有数据的种类，顺序固定（与设置页、侧边栏的顺序相同）。
pub(super) fn count_rows(local: &SyncCounts, remote: &SyncCounts) -> Vec<CountRow> {
    let pick = |c: &SyncCounts| {
        [
            (RecordKind::Host, c.hosts),
            (RecordKind::Group, c.groups),
            (RecordKind::Profile, c.profiles),
            (RecordKind::Key, c.keys),
            (RecordKind::Snippet, c.snippets),
            (RecordKind::Forward, c.forwards),
            (RecordKind::Workspace, c.workspaces),
            (RecordKind::Scheme, c.schemes),
        ]
    };
    pick(local)
        .into_iter()
        .zip(pick(remote))
        .filter(|((_, l), (_, r))| *l > 0 || *r > 0)
        .map(|((kind, local), (_, remote))| CountRow {
            kind,
            local,
            remote,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{self, Language};

    fn conflict(winner: Side, deleted: bool) -> Conflict {
        Conflict {
            key: "host/h1".into(),
            kind: RecordKind::Host,
            name: "web".into(),
            winner,
            deleted,
            can_use_other: true,
        }
    }

    #[test]
    fn resolution_follows_winner_and_deletion() {
        assert_eq!(
            Resolution::of(&conflict(Side::Local, false)),
            Resolution::KeptLocal
        );
        assert_eq!(
            Resolution::of(&conflict(Side::Remote, false)),
            Resolution::KeptRemote
        );
        // 保留了修改的一边胜出：另一边是删除
        let remote_deleted = Resolution::of(&conflict(Side::Local, true));
        assert_eq!(remote_deleted, Resolution::RemoteDeleted);
        assert!(remote_deleted.other_is_delete());
        assert!(Resolution::of(&conflict(Side::Remote, true)).other_is_delete());
        assert!(!Resolution::KeptRemote.other_is_delete());
    }

    #[test]
    fn count_rows_skip_empty_kinds_in_a_fixed_order() {
        let local = SyncCounts {
            hosts: 3,
            keys: 1,
            ..Default::default()
        };
        let remote = SyncCounts {
            hosts: 2,
            snippets: 4,
            ..Default::default()
        };
        let rows = count_rows(&local, &remote);
        let kinds: Vec<RecordKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [RecordKind::Host, RecordKind::Key, RecordKind::Snippet]
        );
        assert_eq!((rows[0].local, rows[0].remote), (3, 2));
        assert_eq!((rows[2].local, rows[2].remote), (0, 4));
        assert!(count_rows(&SyncCounts::default(), &SyncCounts::default()).is_empty());
    }

    #[test]
    fn settings_groups_use_interface_names() {
        let original = i18n::language();
        i18n::set_language(Language::En);
        let mut c = conflict(Side::Local, false);
        c.kind = RecordKind::Settings;
        c.name = "keybindings".into();
        assert_eq!(conflict_name(&c), "Shortcuts");
        c.name = "snippets".into();
        assert_eq!(conflict_name(&c), "Snippet group order");
        c.name = "future_group".into();
        assert_eq!(conflict_name(&c), "future_group", "不认识的组照原样显示");
        c.kind = RecordKind::Host;
        c.name = "general".into();
        assert_eq!(conflict_name(&c), "general", "条目的名称不翻译");
        i18n::set_language(original);
    }
}
