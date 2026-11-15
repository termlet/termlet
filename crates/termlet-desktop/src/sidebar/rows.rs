//! 主机树的扁平行：把工作区、本机的终端、分组与主机排成一列，界面按行渲染。
//!
//! - 最上面是可以折叠的两节："工作区"（保存的分屏布局）与"本机"（默认的本地终端与设置中选的几个），没有时不显示。
//!   "收藏""主机"两节的标题与它们相同（折叠箭头与数量，可以折叠）。
//! - 分组与这四节默认折叠：只有记在展开集合中的（[`RowOptions::expanded`]）才展开，新建的、同步过来的分组折叠着。
//! - 没有搜索时接着是"收藏"（按名称），然后是主机树：最外层先是分组（子分组在前，本组的主机在后，同级按排序），
//!   再是不在分组中的主机（与资源管理器、Xshell 的会话树一样，不另加"未分组"的标题）。上面有工作区、本机或收藏时，
//!   主机树前面加一个"主机"标题。
//! - 拖动时（有分组的话）最下面多一行"放到这里移出分组"；"主机"标题与最外层的主机前后也可以放下（移到最外层）。
//! - 搜索时：工作区与本机的终端按名称过滤（不折叠）；主机不显示分组与收藏，按名称平铺匹配的。折叠状态不变，
//!   清空搜索后照旧。
//! - 上级分组不存在的分组当作顶层，分组不存在的主机当作不在分组中。

use std::collections::{BTreeSet, HashMap, HashSet};

use termlet_core::id::{GroupId, HostId, WorkspaceId};
use termlet_core::model::{Group, Host, Protocol, RemoteOs, Workspace};
use termlet_core::store::{Position, Record};

use crate::brand_icons::Brand;

/// 行的身份（选中、拖放目标按它认，不按位置）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// "工作区"一节的标题
    Workspaces,
    Workspace(WorkspaceId),
    /// "本机"一节的标题
    Local,
    /// 本机的终端（设置中的写法：`local:<shell ID>` 或 `wsl:<发行版>`）
    LocalShell(String),
    /// 上面有工作区、本机或收藏时主机树的标题（拖到它上面：移到最外层）
    Hosts,
    Favorites,
    /// 拖动时最下面的放置位置：移出分组（移到最外层）
    TopLevel,
    Group(GroupId),
    /// `favorite` 为真时是收藏区里的那一行（同一台主机在原分组里还有一行）
    Host {
        id: HostId,
        favorite: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowKind {
    Section,
    /// 可以折叠的一节的标题（工作区、本机、收藏、主机）
    Heading {
        count: usize,
        collapsed: bool,
    },
    Workspace {
        name: String,
        panes: usize,
    },
    LocalShell {
        name: String,
        /// 行首的标志（shell、WSL 发行版）；没有时是终端的图标
        brand: Option<Brand>,
        is_default: bool,
        /// WSL 发行版（名称后标"WSL"，有文件管理）
        wsl: bool,
        /// 平时显示在侧边栏中（右键菜单是"从侧边栏移除"还是"在侧边栏中显示"）
        shown: bool,
    },
    Group {
        name: String,
        /// 包含各级子分组中的主机
        count: usize,
        collapsed: bool,
    },
    Host {
        name: String,
        /// 行首的图标按协议区分
        protocol: Protocol,
        /// 识别出的系统（只有 SSH 主机）：行首换成系统的标志
        os: Option<RemoteOs>,
        /// 名称下方的 `用户名@地址`，Telnet、串口带协议（设置中关闭时为 `None`）
        address: Option<String>,
        tags: Vec<String>,
        favorite: bool,
        /// 悬停提示：`user@host:port`，串口带参数
        endpoint: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: RowKey,
    /// 缩进的层数
    pub depth: usize,
    pub kind: RowKind,
}

impl Row {
    pub fn is_section(&self) -> bool {
        matches!(self.kind, RowKind::Section)
    }
}

/// "本机"一节中的一项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalEntry {
    /// 设置中的写法：`local:<shell ID>` 或 `wsl:<发行版>`
    pub key: String,
    pub name: String,
    /// 行首的标志；没有时是终端的图标
    pub brand: Option<Brand>,
    pub is_default: bool,
    pub wsl: bool,
    /// 平时显示在侧边栏中（默认的一项与设置中勾选的）；其余的只在搜索时列出
    pub shown: bool,
}

/// 展开状态中各节的键（与分组 ID 不会重名，都以 [`SECTION_PREFIX`] 开头）。
pub const SECTION_PREFIX: &str = "section:";
pub const WORKSPACES_SECTION: &str = "section:workspaces";
pub const LOCAL_SECTION: &str = "section:local";
pub const FAVORITES_SECTION: &str = "section:favorites";
pub const HOSTS_SECTION: &str = "section:hosts";

/// 生成行时需要的选项。
pub struct RowOptions<'a> {
    /// 展开着的分组与节（分组 ID、节的键）；没有记的折叠
    pub expanded: &'a BTreeSet<String>,
    pub workspaces: &'a [Record<Workspace>],
    pub locals: &'a [LocalEntry],
    pub query: &'a str,
    pub show_address: bool,
    /// 正在拖动：有分组时最下面显示"放到这里移出分组"
    pub dragging: bool,
}

pub fn build_rows(
    groups: &[Record<Group>],
    hosts: &[Record<Host>],
    options: &RowOptions<'_>,
) -> Vec<Row> {
    let query = options.query.trim();
    let mut rows = top_sections(options, query);
    let has_sections = !rows.is_empty();
    if !query.is_empty() {
        let mut matched: Vec<&Record<Host>> =
            hosts.iter().filter(|h| h.doc.matches(query)).collect();
        matched.sort_by_key(|h| h.doc.name.to_lowercase());
        if has_sections && !matched.is_empty() {
            rows.push(heading(RowKey::Hosts, matched.len(), false));
        }
        rows.extend(
            matched
                .into_iter()
                .map(|h| host_row(h, 0, false, options.show_address)),
        );
        return rows;
    }
    let group_ids: HashSet<&str> = groups.iter().map(|g| g.id.as_str()).collect();
    let mut child_groups: HashMap<Option<&str>, Vec<&Record<Group>>> = HashMap::new();
    for group in groups {
        child_groups
            .entry(parent_of(&group.parent, &group_ids))
            .or_default()
            .push(group);
    }
    let mut group_hosts: HashMap<Option<&str>, Vec<&Record<Host>>> = HashMap::new();
    for host in hosts {
        group_hosts
            .entry(parent_of(&host.parent, &group_ids))
            .or_default()
            .push(host);
    }
    let counts = host_counts(groups, &child_groups, &group_hosts);

    let mut favorites: Vec<&Record<Host>> = hosts.iter().filter(|h| h.doc.favorite).collect();
    let has_favorites = !favorites.is_empty();
    if has_favorites {
        favorites.sort_by_key(|h| h.doc.name.to_lowercase());
        let collapsed = !options.expanded.contains(FAVORITES_SECTION);
        rows.push(heading(RowKey::Favorites, favorites.len(), collapsed));
        if !collapsed {
            rows.extend(
                favorites
                    .into_iter()
                    .map(|h| host_row(h, 0, true, options.show_address)),
            );
        }
    }
    // 上面有别的内容时，用标题把主机树与它们分开（收藏里的主机在树中还有一行）；折叠时不列出树
    if (has_sections || has_favorites) && (!hosts.is_empty() || !groups.is_empty()) {
        let collapsed = !options.expanded.contains(HOSTS_SECTION);
        rows.push(heading(RowKey::Hosts, hosts.len(), collapsed));
        if collapsed {
            if options.dragging && !groups.is_empty() {
                rows.push(section(RowKey::TopLevel));
            }
            return rows;
        }
    }

    let mut walk = vec![];
    if let Some(top) = child_groups.get(&None) {
        walk.extend(top.iter().rev().map(|g| (*g, 0usize)));
    }
    // 先序遍历：分组行，展开时接着是子分组（递归）与本组的主机
    let mut stack: Vec<Item<'_>> = walk.into_iter().map(|(g, d)| Item::Group(g, d)).collect();
    while let Some(item) = stack.pop() {
        match item {
            Item::Group(group, depth) => {
                let collapsed = !options.expanded.contains(group.id.as_str());
                rows.push(Row {
                    key: RowKey::Group(group.id.clone()),
                    depth,
                    kind: RowKind::Group {
                        name: group.doc.name.clone(),
                        count: counts.get(group.id.as_str()).copied().unwrap_or(0),
                        collapsed,
                    },
                });
                if collapsed {
                    continue;
                }
                let key = Some(group.id.as_str());
                // 栈是后进先出：先压主机（后显示），再倒序压子分组
                if let Some(hosts) = group_hosts.get(&key) {
                    stack.extend(hosts.iter().rev().map(|h| Item::Host(h, depth + 1)));
                }
                if let Some(children) = child_groups.get(&key) {
                    stack.extend(children.iter().rev().map(|g| Item::Group(g, depth + 1)));
                }
            }
            Item::Host(host, depth) => {
                rows.push(host_row(host, depth, false, options.show_address))
            }
        }
    }

    // 不在分组中的主机：在最外层，排在分组之后
    let top_level = group_hosts
        .get(&None)
        .map(Vec::as_slice)
        .unwrap_or_default();
    rows.extend(
        top_level
            .iter()
            .map(|h| host_row(h, 0, false, options.show_address)),
    );
    if options.dragging && !groups.is_empty() {
        rows.push(section(RowKey::TopLevel));
    }
    rows
}

/// 最上面的两节：工作区、本机。搜索时只列出名称匹配的，不折叠。
fn top_sections(options: &RowOptions<'_>, query: &str) -> Vec<Row> {
    let query = query.to_lowercase();
    let matches = |name: &str| query.is_empty() || name.to_lowercase().contains(&query);
    let mut rows = Vec::new();
    let mut workspaces: Vec<&Record<Workspace>> = options
        .workspaces
        .iter()
        .filter(|w| matches(&w.doc.name))
        .collect();
    workspaces.sort_by_key(|w| w.doc.name.to_lowercase());
    if !workspaces.is_empty() {
        let collapsed = query.is_empty() && !options.expanded.contains(WORKSPACES_SECTION);
        rows.push(Row {
            key: RowKey::Workspaces,
            depth: 0,
            kind: RowKind::Heading {
                count: workspaces.len(),
                collapsed,
            },
        });
        if !collapsed {
            rows.extend(workspaces.into_iter().map(|w| Row {
                key: RowKey::Workspace(w.id.clone()),
                depth: 0,
                kind: RowKind::Workspace {
                    name: w.doc.name.clone(),
                    panes: w.doc.hosts().len(),
                },
            }));
        }
    }
    // 平时只列显示在侧边栏中的；搜索时没显示的也列出（可以从这里打开，或右键"在侧边栏中显示"）
    let locals: Vec<&LocalEntry> = options
        .locals
        .iter()
        .filter(|l| (l.shown || !query.is_empty()) && matches(&l.name))
        .collect();
    if !locals.is_empty() {
        let collapsed = query.is_empty() && !options.expanded.contains(LOCAL_SECTION);
        rows.push(Row {
            key: RowKey::Local,
            depth: 0,
            kind: RowKind::Heading {
                count: locals.len(),
                collapsed,
            },
        });
        if !collapsed {
            rows.extend(locals.into_iter().map(|l| Row {
                key: RowKey::LocalShell(l.key.clone()),
                depth: 0,
                kind: RowKind::LocalShell {
                    name: l.name.clone(),
                    brand: l.brand,
                    is_default: l.is_default,
                    wsl: l.wsl,
                    shown: l.shown,
                },
            }));
        }
    }
    rows
}

/// 所在的分组；分组不存在的当作在最外层。
fn parent_of<'a>(parent: &'a Option<String>, groups: &HashSet<&str>) -> Option<&'a str> {
    parent.as_deref().filter(|p| groups.contains(p))
}

enum Item<'a> {
    Group(&'a Record<Group>, usize),
    Host(&'a Record<Host>, usize),
}

fn section(key: RowKey) -> Row {
    Row {
        key,
        depth: 0,
        kind: RowKind::Section,
    }
}

fn heading(key: RowKey, count: usize, collapsed: bool) -> Row {
    Row {
        key,
        depth: 0,
        kind: RowKind::Heading { count, collapsed },
    }
}

fn host_row(host: &Record<Host>, depth: usize, in_favorites: bool, show_address: bool) -> Row {
    let doc = &host.doc;
    Row {
        key: RowKey::Host {
            id: host.id.clone(),
            favorite: in_favorites,
        },
        depth,
        kind: RowKind::Host {
            name: doc.name.clone(),
            protocol: doc.protocol,
            os: doc.is_ssh().then(|| doc.remote_os()).flatten(),
            address: show_address.then(|| crate::host_info::summary(doc)),
            tags: doc.tags.clone(),
            favorite: doc.favorite,
            endpoint: crate::host_info::endpoint(doc),
        },
    }
}

/// 每个分组（含各级子分组）中的主机数。
fn host_counts<'a>(
    groups: &'a [Record<Group>],
    child_groups: &HashMap<Option<&'a str>, Vec<&'a Record<Group>>>,
    group_hosts: &HashMap<Option<&'a str>, Vec<&'a Record<Host>>>,
) -> HashMap<&'a str, usize> {
    fn count<'a>(
        id: &'a str,
        child_groups: &HashMap<Option<&'a str>, Vec<&'a Record<Group>>>,
        group_hosts: &HashMap<Option<&'a str>, Vec<&'a Record<Host>>>,
        memo: &mut HashMap<&'a str, usize>,
        depth: usize,
    ) -> usize {
        if let Some(n) = memo.get(id) {
            return *n;
        }
        // 存储保证没有循环；这里只防御数据异常时的无限递归
        if depth > 64 {
            return 0;
        }
        let own = group_hosts.get(&Some(id)).map_or(0, Vec::len);
        let children: usize = child_groups
            .get(&Some(id))
            .map(|c| {
                c.iter()
                    .map(|g| count(g.id.as_str(), child_groups, group_hosts, memo, depth + 1))
                    .sum()
            })
            .unwrap_or(0);
        memo.insert(id, own + children);
        own + children
    }
    let mut memo = HashMap::new();
    for group in groups {
        count(group.id.as_str(), child_groups, group_hosts, &mut memo, 0);
    }
    memo
}

/// 分组 `ancestor` 是否是 `group` 自己或它的上级（拖动分组时不能放进自己或子孙）。
pub fn is_self_or_ancestor(
    groups: &[Record<Group>],
    ancestor: &GroupId,
    group: Option<&GroupId>,
) -> bool {
    let parents: HashMap<&str, Option<&str>> = groups
        .iter()
        .map(|g| (g.id.as_str(), g.parent.as_deref()))
        .collect();
    let mut next = group.map(GroupId::as_str);
    let mut steps = 0;
    while let Some(id) = next {
        if id == ancestor.as_str() {
            return true;
        }
        steps += 1;
        if steps > 64 {
            return false;
        }
        next = parents.get(id).copied().flatten();
    }
    false
}

// ------------------------------------------------------------------ 拖放

/// 正在拖动的条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dragged {
    Host(HostId),
    Group(GroupId),
}

/// 放在目标行的哪里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropPlace {
    Before,
    /// 放进分组（或移出分组：目标是"主机"标题或最下面的放置位置）
    Into,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropTarget {
    pub row: RowKey,
    pub place: DropPlace,
}

/// 拖到 `row` 上、指针在行内的相对高度为 `y`（0 在上沿、1 在下沿）时放在哪里；不能放时为 `None`。
///
/// - 主机拖到分组行上：放进分组；拖到主机行上：上半在前、下半在后（进入那台主机所在的分组）。
/// - 分组拖到分组行上：上 1/4 在前；下 1/4 且目标折叠着时在后；其余放进去成为子分组。
/// - 拖到"主机"标题或拖动时最下面的那一行上：移出分组（移到最外层的最后）。
/// - 收藏区不能放；主机不能放在分组的前后，分组不能放在主机上，分组不能放进自己或子孙。
pub fn drop_target(
    dragged: &Dragged,
    row: &Row,
    y: f32,
    groups: &[Record<Group>],
) -> Option<DropTarget> {
    let place = match (&row.key, &row.kind, dragged) {
        (RowKey::TopLevel | RowKey::Hosts, _, _) => DropPlace::Into,
        (RowKey::Favorites, _, _) | (RowKey::Host { favorite: true, .. }, _, _) => return None,
        (RowKey::Group(_), _, Dragged::Host(_)) => DropPlace::Into,
        (RowKey::Group(target), RowKind::Group { collapsed, .. }, Dragged::Group(moving)) => {
            let place = if y < 0.25 {
                DropPlace::Before
            } else if y > 0.75 && *collapsed {
                DropPlace::After
            } else {
                DropPlace::Into
            };
            let new_parent = match place {
                DropPlace::Into => Some(target.clone()),
                _ => parent_group(groups, target),
            };
            if target == moving || is_self_or_ancestor(groups, moving, new_parent.as_ref()) {
                return None;
            }
            place
        }
        (RowKey::Host { id, .. }, _, Dragged::Host(moving)) => {
            if id == moving {
                return None;
            }
            if y < 0.5 {
                DropPlace::Before
            } else {
                DropPlace::After
            }
        }
        _ => return None,
    };
    Some(DropTarget {
        row: row.key.clone(),
        place,
    })
}

/// 放下后的移动：移到哪个分组（`None` 为最外层）、排在哪里。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Move {
    Host {
        id: HostId,
        parent: Option<GroupId>,
        position: Position<HostId>,
    },
    Group {
        id: GroupId,
        parent: Option<GroupId>,
        position: Position<GroupId>,
    },
}

pub fn plan_move(
    dragged: &Dragged,
    target: &DropTarget,
    groups: &[Record<Group>],
    hosts: &[Record<Host>],
) -> Option<Move> {
    match (dragged, &target.row, target.place) {
        (Dragged::Host(id), RowKey::TopLevel | RowKey::Hosts, _) => Some(Move::Host {
            id: id.clone(),
            parent: None,
            position: Position::Last,
        }),
        (Dragged::Group(id), RowKey::TopLevel | RowKey::Hosts, _) => Some(Move::Group {
            id: id.clone(),
            parent: None,
            position: Position::Last,
        }),
        (Dragged::Host(id), RowKey::Group(group), _) => Some(Move::Host {
            id: id.clone(),
            parent: Some(group.clone()),
            position: Position::Last,
        }),
        (Dragged::Host(id), RowKey::Host { id: beside, .. }, place) => {
            let parent = hosts
                .iter()
                .find(|h| &h.id == beside)?
                .parent
                .clone()
                .map(GroupId::from_raw)
                .filter(|g| groups.iter().any(|r| &r.id == g));
            Some(Move::Host {
                id: id.clone(),
                parent,
                position: if place == DropPlace::Before {
                    Position::Before(beside.clone())
                } else {
                    Position::After(beside.clone())
                },
            })
        }
        (Dragged::Group(id), RowKey::Group(group), DropPlace::Into) => Some(Move::Group {
            id: id.clone(),
            parent: Some(group.clone()),
            position: Position::Last,
        }),
        (Dragged::Group(id), RowKey::Group(group), place) => Some(Move::Group {
            id: id.clone(),
            parent: parent_group(groups, group),
            position: if place == DropPlace::Before {
                Position::Before(group.clone())
            } else {
                Position::After(group.clone())
            },
        }),
        _ => None,
    }
}

fn parent_group(groups: &[Record<Group>], group: &GroupId) -> Option<GroupId> {
    let parent = groups.iter().find(|g| &g.id == group)?.parent.clone()?;
    groups
        .iter()
        .any(|g| g.id.as_str() == parent)
        .then(|| GroupId::from_raw(parent))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, parent: Option<&str>, name: &str, sort: &str) -> Record<Group> {
        Record {
            id: GroupId::from_raw(id),
            parent: parent.map(str::to_string),
            sort_key: sort.into(),
            doc: Group {
                name: name.into(),
                ..Default::default()
            },
            created_at: 0,
            updated_at: 0,
        }
    }

    fn host(id: &str, parent: Option<&str>, name: &str, favorite: bool) -> Record<Host> {
        Record {
            id: HostId::from_raw(id),
            parent: parent.map(str::to_string),
            sort_key: "m".into(),
            doc: Host {
                name: name.into(),
                address: format!("{name}.lan"),
                username: "ops".into(),
                favorite,
                ..Default::default()
            },
            created_at: 0,
            updated_at: 0,
        }
    }

    fn keys(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r.key {
                RowKey::Workspaces => "[工作区]".to_string(),
                RowKey::Workspace(id) => id.as_str().to_string(),
                RowKey::Local => "[本机]".to_string(),
                RowKey::LocalShell(key) => key.clone(),
                RowKey::Hosts => "[主机]".to_string(),
                RowKey::Favorites => "[收藏]".to_string(),
                RowKey::TopLevel => "[移出分组]".to_string(),
                RowKey::Group(id) => format!("{}{}", "  ".repeat(r.depth), id.as_str()),
                RowKey::Host { id, favorite } => {
                    format!(
                        "{}{}{}",
                        "  ".repeat(r.depth),
                        id.as_str(),
                        if *favorite { "*" } else { "" }
                    )
                }
            })
            .collect()
    }

    fn options(expanded: &BTreeSet<String>) -> RowOptions<'_> {
        RowOptions {
            expanded,
            workspaces: &[],
            locals: &[],
            query: "",
            show_address: true,
            dragging: false,
        }
    }

    /// 全部展开：各节与 `groups` 中的分组。
    fn all_expanded(groups: &[Record<Group>]) -> BTreeSet<String> {
        [
            WORKSPACES_SECTION,
            LOCAL_SECTION,
            FAVORITES_SECTION,
            HOSTS_SECTION,
        ]
        .into_iter()
        .map(str::to_string)
        .chain(groups.iter().map(|g| g.id.as_str().to_string()))
        .collect()
    }

    fn workspace(id: &str, name: &str) -> Record<Workspace> {
        Record {
            id: WorkspaceId::from_raw(id),
            parent: None,
            sort_key: "m".into(),
            doc: Workspace {
                name: name.into(),
                ..Default::default()
            },
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn workspaces_and_local_come_first_and_collapse() {
        let workspaces = vec![workspace("w_b", "部署"), workspace("w_a", "巡检")];
        let locals = vec![
            LocalEntry {
                key: "local:pwsh".into(),
                name: "PowerShell".into(),
                brand: None,
                is_default: true,
                wsl: false,
                shown: true,
            },
            LocalEntry {
                key: "local:gitbash".into(),
                name: "Git Bash".into(),
                brand: None,
                is_default: false,
                wsl: false,
                shown: true,
            },
            LocalEntry {
                key: "local:cmd".into(),
                name: "Command Prompt".into(),
                brand: None,
                is_default: false,
                wsl: false,
                shown: false,
            },
        ];
        let hosts = vec![host("h_a", None, "a", false)];
        let all = all_expanded(&[]);
        let opts = RowOptions {
            workspaces: &workspaces,
            locals: &locals,
            ..options(&all)
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &opts)),
            [
                "[工作区]",
                "w_a",
                "w_b",
                "[本机]",
                "local:pwsh",
                "local:gitbash",
                "[主机]",
                "h_a"
            ]
        );
        // 折叠的节只留标题
        let mut folded = all.clone();
        folded.remove(WORKSPACES_SECTION);
        let opts = RowOptions {
            expanded: &folded,
            ..opts
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &opts))[..2],
            ["[工作区]", "[本机]"]
        );
        // 默认（没有记展开的）全部折叠：只有各节的标题
        let none = BTreeSet::new();
        let default = RowOptions {
            expanded: &none,
            ..opts
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &default)),
            ["[工作区]", "[本机]", "[主机]"]
        );
        // 搜索时按名称过滤、不折叠
        let search = RowOptions {
            query: "git",
            ..opts
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &search)),
            ["[本机]", "local:gitbash"]
        );
        // 没显示在侧边栏中的只在搜索时列出
        let search = RowOptions {
            query: "command",
            ..opts
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &search)),
            ["[本机]", "local:cmd"]
        );
        // 没有这两节时不加"主机"标题
        assert_eq!(keys(&build_rows(&[], &hosts, &options(&none))), ["h_a"]);
        // 搜索时节不折叠（默认折叠也一样）
        let search = RowOptions {
            query: "git",
            ..default
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &search)),
            ["[本机]", "local:gitbash"]
        );
    }

    #[test]
    fn tree_lists_favorites_groups_then_top_level_hosts() {
        let groups = vec![
            group("g_prod", None, "生产", "a"),
            group("g_web", Some("g_prod"), "web", "a"),
            group("g_orphan", Some("g_gone"), "孤儿", "b"),
        ];
        let hosts = vec![
            host("h_db", Some("g_prod"), "db", true),
            host("h_web1", Some("g_web"), "web1", false),
            host("h_loose", None, "loose", false),
            host("h_lost", Some("g_gone"), "lost", false),
        ];
        let all = all_expanded(&groups);
        let rows = build_rows(&groups, &hosts, &options(&all));
        assert_eq!(
            keys(&rows),
            [
                "[收藏]",
                "h_db*",
                "[主机]",
                "g_prod",
                "  g_web",
                "    h_web1",
                "  h_db",
                "g_orphan",
                "h_loose",
                "h_lost",
            ],
            "不在分组中的主机在最外层、分组之后，没有“未分组”的标题"
        );
        match &rows[3].kind {
            RowKind::Group { count, .. } => assert_eq!(*count, 2, "包含子分组中的主机"),
            other => panic!("{other:?}"),
        }
        match &rows[5].kind {
            RowKind::Host {
                address,
                endpoint,
                protocol,
                ..
            } => {
                assert_eq!(address.as_deref(), Some("ops@web1.lan"));
                assert_eq!(endpoint, "ops@web1.lan:22");
                assert_eq!(*protocol, Protocol::Ssh);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn favorites_and_hosts_sections_collapse() {
        let groups = vec![group("g", None, "g", "a")];
        let hosts = vec![
            host("h_a", Some("g"), "a", true),
            host("h_b", None, "b", false),
        ];
        let mut expanded = all_expanded(&groups);
        expanded.remove(FAVORITES_SECTION);
        let rows = build_rows(&groups, &hosts, &options(&expanded));
        assert_eq!(keys(&rows), ["[收藏]", "[主机]", "g", "  h_a", "h_b"]);
        assert_eq!(
            rows[0].kind,
            RowKind::Heading {
                count: 1,
                collapsed: true
            }
        );
        assert_eq!(
            rows[1].kind,
            RowKind::Heading {
                count: 2,
                collapsed: false
            }
        );
        let mut expanded = all_expanded(&groups);
        expanded.remove(HOSTS_SECTION);
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &options(&expanded))),
            ["[收藏]", "h_a*", "[主机]"],
            "折叠主机只留标题"
        );
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &options(&BTreeSet::new()))),
            ["[收藏]", "[主机]"],
            "默认两节都折叠"
        );
    }

    #[test]
    fn collapsed_groups_hide_children_and_search_flattens() {
        let groups = vec![group("g_prod", None, "生产", "a")];
        let hosts = vec![
            host("h_b", Some("g_prod"), "Beta", false),
            host("h_a", None, "alpha", false),
        ];
        // 没有记展开的分组折叠着（新建的、同步过来的分组也是）
        let empty = BTreeSet::new();
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &options(&empty))),
            ["g_prod", "h_a"]
        );
        let expanded = all_expanded(&groups);
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &options(&expanded))),
            ["g_prod", "  h_b", "h_a"]
        );

        let search = RowOptions {
            query: " A ",
            ..options(&empty)
        };
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &search)),
            ["h_a", "h_b"],
            "按名称排列，不显示分组"
        );
    }

    #[test]
    fn top_level_drop_row_only_while_dragging_with_groups() {
        let empty = BTreeSet::new();
        let hosts = vec![host("h_a", None, "a", false)];
        let dragging = RowOptions {
            dragging: true,
            ..options(&empty)
        };
        assert_eq!(
            keys(&build_rows(&[], &hosts, &dragging)),
            ["h_a"],
            "没有分组时不用移出"
        );
        let groups = vec![group("g", None, "g", "a")];
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &dragging)),
            ["g", "h_a", "[移出分组]"]
        );
        assert_eq!(
            keys(&build_rows(&groups, &hosts, &options(&empty))),
            ["g", "h_a"]
        );
    }

    #[test]
    fn ancestor_check_prevents_moving_into_descendants() {
        let groups = vec![
            group("a", None, "a", "a"),
            group("b", Some("a"), "b", "a"),
            group("c", Some("b"), "c", "a"),
        ];
        let a = GroupId::from_raw("a");
        assert!(is_self_or_ancestor(
            &groups,
            &a,
            Some(&GroupId::from_raw("c"))
        ));
        assert!(is_self_or_ancestor(&groups, &a, Some(&a)));
        assert!(!is_self_or_ancestor(
            &groups,
            &GroupId::from_raw("c"),
            Some(&a)
        ));
        assert!(!is_self_or_ancestor(&groups, &a, None));
    }

    #[test]
    fn drop_rules_follow_row_kind_and_height() {
        let groups = vec![group("a", None, "a", "a"), group("b", Some("a"), "b", "b")];
        let hosts = vec![
            host("h1", Some("a"), "h1", false),
            host("h2", None, "h2", false),
        ];
        let expanded = all_expanded(&groups);
        let rows = build_rows(&groups, &hosts, &options(&expanded));
        let row = |key: &str| {
            rows.iter()
                .find(|r| keys(std::slice::from_ref(r))[0].trim() == key)
                .unwrap()
                .clone()
        };

        let host = Dragged::Host(HostId::from_raw("h2"));
        // 主机拖到分组上：放进去
        let target = drop_target(&host, &row("a"), 0.1, &groups).unwrap();
        assert_eq!(target.place, DropPlace::Into);
        assert_eq!(
            plan_move(&host, &target, &groups, &hosts),
            Some(Move::Host {
                id: HostId::from_raw("h2"),
                parent: Some(GroupId::from_raw("a")),
                position: Position::Last
            })
        );
        // 拖到主机行的上半：排在它前面，进入它的分组
        let target = drop_target(&host, &row("h1"), 0.3, &groups).unwrap();
        assert_eq!(
            plan_move(&host, &target, &groups, &hosts),
            Some(Move::Host {
                id: HostId::from_raw("h2"),
                parent: Some(GroupId::from_raw("a")),
                position: Position::Before(HostId::from_raw("h1"))
            })
        );
        assert!(
            drop_target(&host, &row("h2"), 0.3, &groups).is_none(),
            "不能放在自己身上"
        );

        let group_a = Dragged::Group(GroupId::from_raw("a"));
        assert!(
            drop_target(&group_a, &row("b"), 0.5, &groups).is_none(),
            "不能放进自己的子分组"
        );
        assert!(
            drop_target(&group_a, &row("h1"), 0.5, &groups).is_none(),
            "分组不能放在主机上"
        );
        let group_b = Dragged::Group(GroupId::from_raw("b"));
        let target = drop_target(&group_b, &row("a"), 0.1, &groups).unwrap();
        assert_eq!(target.place, DropPlace::Before);
        assert_eq!(
            plan_move(&group_b, &target, &groups, &hosts),
            Some(Move::Group {
                id: GroupId::from_raw("b"),
                parent: None,
                position: Position::Before(GroupId::from_raw("a"))
            })
        );
        // 展开着的分组下 1/4 也是放进去（放在后面会和它的子项混在一起）
        assert_eq!(
            drop_target(&group_b, &row("a"), 0.9, &groups)
                .unwrap()
                .place,
            DropPlace::Into
        );
        // 拖动时最下面的那一行、"主机"标题：移出分组，到最外层的最后
        let dragging_rows = build_rows(
            &groups,
            &hosts,
            &RowOptions {
                dragging: true,
                ..options(&expanded)
            },
        );
        let drop_row = dragging_rows.last().unwrap();
        assert_eq!(drop_row.key, RowKey::TopLevel);
        let target = drop_target(&group_b, drop_row, 0.5, &groups).unwrap();
        assert_eq!(
            plan_move(&group_b, &target, &groups, &hosts),
            Some(Move::Group {
                id: GroupId::from_raw("b"),
                parent: None,
                position: Position::Last
            })
        );
        let heading = heading(RowKey::Hosts, hosts.len(), false);
        let h1 = Dragged::Host(HostId::from_raw("h1"));
        let target = drop_target(&h1, &heading, 0.5, &groups).unwrap();
        assert_eq!(
            plan_move(&h1, &target, &groups, &hosts),
            Some(Move::Host {
                id: HostId::from_raw("h1"),
                parent: None,
                position: Position::Last
            })
        );
        // 放在最外层的主机前后：移到最外层
        let target = drop_target(&h1, &row("h2"), 0.7, &groups).unwrap();
        assert_eq!(
            plan_move(&h1, &target, &groups, &hosts),
            Some(Move::Host {
                id: HostId::from_raw("h1"),
                parent: None,
                position: Position::After(HostId::from_raw("h2"))
            })
        );
    }
}
