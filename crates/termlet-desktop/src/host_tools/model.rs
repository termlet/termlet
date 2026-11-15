//! 主机工具面板的纯逻辑：停靠栏的宽度与当前页、读取结果的暂存与恢复、
//! 各页的排序、搜索、筛选、分组与确认的分级，以及每个终端各页上次结果的缓存。界面见同目录的各页。

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;
use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;
use termlet_core::hosttools::{
    Conn, ConnList, ConnProcess, Container, DockerInfo, HealthState, Image, MainExit, Network,
    PortBinding, Process, ProcessList, ServerZone, Service, ServiceOp, Timer, Volume,
};
use termlet_core::monitor::Details;
use termlet_core::time::{DateStyle, format_compact, format_local};

use crate::sftp::list_model::{natural_cmp, visible_name};

// ------------------------------------------------------------ 停靠栏

/// 停靠栏的宽度（逻辑像素，与侧边栏一样记在本机的界面状态中）。这几个宽度与记下的宽度都按 100% 的界面大小，用时
/// 乘上界面的缩放倍数（[`crate::ui_scale::factor`]）。
pub const WIDTH_DEFAULT: f32 = 400.;
pub const WIDTH_MIN: f32 = 320.;
pub const WIDTH_MAX: f32 = 760.;
/// 左侧（侧边栏与终端区域）至少保留的宽度
pub const REST_MIN: f32 = 480.;
/// 拖动条获得焦点时左右方向键每次调整的宽度
pub const WIDTH_STEP: f32 = 16.;

/// 限制在可用范围内（像素，界面的缩放倍数为 `scale`）：不小于最小宽度 `min`（[`WIDTH_MIN`] 乘上 `scale`，页签按文案
/// 放不下时更宽），不超过最大宽度，并给左侧留出 [`REST_MIN`]（窗口太窄时以最小宽度为准）；取整。
pub fn clamp_width(width: f32, window_width: f32, min: f32, scale: f32) -> f32 {
    let max = min.max((WIDTH_MAX * scale).min(window_width - REST_MIN * scale));
    width.max(min).min(max).round()
}

/// 记下的宽度：没有、或不在最小与最大之间时用默认宽度。
pub fn saved_width(saved: Option<f32>) -> f32 {
    saved
        .filter(|w| w.is_finite() && (WIDTH_MIN..=WIDTH_MAX).contains(w))
        .unwrap_or(WIDTH_DEFAULT)
}

/// 面板的各页（按页签的顺序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Overview,
    Processes,
    Services,
    Docker,
    Network,
}

impl Page {
    pub const ALL: [Page; 5] = [
        Page::Overview,
        Page::Processes,
        Page::Services,
        Page::Docker,
        Page::Network,
    ];

    /// 记在界面状态中的名称。
    pub fn key(self) -> &'static str {
        match self {
            Page::Overview => "overview",
            Page::Processes => "processes",
            Page::Services => "services",
            Page::Docker => "docker",
            Page::Network => "network",
        }
    }

    /// 记下的页；没有或不认识时为第一页（概览）。
    pub fn from_saved(saved: Option<&str>) -> Page {
        Page::ALL
            .into_iter()
            .find(|p| Some(p.key()) == saved)
            .unwrap_or(Page::Overview)
    }

    pub fn index(self) -> usize {
        Page::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

// ------------------------------------------------------------ 键盘

/// 键盘移动当前行的幅度。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Prev,
    Next,
    PageUp,
    PageDown,
    First,
    Last,
}

/// 按 `step` 移动当前行：`current` 是当前行（没有时 ↑ ↓ 等都到第一个能选的行），`page` 是一页的行数。
/// `selectable` 为假的行（Docker 的项目标题）跳过：往上移时找上面能选的，上面没有时往下找（往下移同理）。
/// 没有能选的行时为 `None`。
pub fn step_row(
    current: Option<usize>,
    len: usize,
    step: Step,
    page: usize,
    selectable: impl Fn(usize) -> bool,
) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let page = page.max(1);
    let (target, down) = match (step, current.map(|ix| ix.min(last))) {
        (Step::First, _) | (_, None) => (0, true),
        (Step::Last, _) => (last, false),
        (Step::Prev, Some(ix)) => (ix.saturating_sub(1), false),
        (Step::Next, Some(ix)) => ((ix + 1).min(last), true),
        (Step::PageUp, Some(ix)) => (ix.saturating_sub(page), false),
        (Step::PageDown, Some(ix)) => ((ix + page).min(last), true),
    };
    let below = || (target..len).find(|&ix| selectable(ix));
    let above = || (0..=target).rev().find(|&ix| selectable(ix));
    if down {
        below().or_else(above)
    } else {
        above().or_else(below)
    }
}

/// PageUp / PageDown 一次移动的行数：看得见的行数减一（至少一行）。
pub fn page_rows(viewport: f32, row: f32) -> usize {
    if row <= 0. {
        return 1;
    }
    ((viewport / row).floor() as usize).saturating_sub(1).max(1)
}

// ------------------------------------------------------------ 读取结果

/// 一页显示的数据与读取时间（“更新于”），以及暂停期间读到、还没显示的结果。
/// 切回某个终端时先显示缓存（没有读取时间），再重新读取。
pub struct Shown<T> {
    pub data: Option<Rc<T>>,
    /// 读取时间（Unix 毫秒）；显示的是缓存时为 `None`
    pub at: Option<i64>,
    pending: Option<(Rc<T>, i64)>,
}

impl<T> Shown<T> {
    pub fn new(cached: Option<Rc<T>>) -> Self {
        Self {
            data: cached,
            at: None,
            pending: None,
        }
    }

    /// 读到新的结果：暂停期间（鼠标停在列表上等）先存着，显示的数据与“更新于”都不变；`force`（自己操作之后的
    /// 重读）时直接显示，并丢掉暂存的旧结果。
    pub fn arrive(&mut self, value: Rc<T>, at: i64, paused: bool, force: bool) {
        if paused && !force {
            self.pending = Some((value, at));
        } else {
            self.data = Some(value);
            self.at = Some(at);
            self.pending = None;
        }
    }

    /// 暂停期间读到、还没显示的结果。
    pub fn pending(&self) -> Option<&Rc<T>> {
        self.pending.as_ref().map(|(value, _)| value)
    }

    /// 换掉显示着的数据，“更新于”与暂存的结果不变（暂停期间把补读到的读数就地填进显示着的行）。
    pub fn patch(&mut self, data: Rc<T>) {
        self.data = Some(data);
    }

    /// 恢复刷新：显示暂停期间读到的结果。
    pub fn resume(&mut self) {
        if let Some((value, at)) = self.pending.take() {
            self.data = Some(value);
            self.at = Some(at);
        }
    }
}

/// 每个终端、每一页上次的结果：切回来先显示它，再重新读取。最近用过的在后，超过上限时丢掉最久没用的；
/// 终端关闭时清除它的各页（[`Lru::forget`]）。
pub struct Lru<K, V> {
    entries: VecDeque<(K, V)>,
    cap: usize,
}

impl<K: PartialEq + Clone, V: Clone> Lru<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cap,
        }
    }

    pub fn get(&self, key: &K) -> Option<V> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }

    /// 记下（已有时移到最后）。
    pub fn put(&mut self, key: K, value: V) {
        self.entries.retain(|(k, _)| k != &key);
        self.entries.push_back((key, value));
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// 清除满足条件的项（一个终端的各页）。
    pub fn forget(&mut self, mut matches: impl FnMut(&K) -> bool) {
        self.entries.retain(|(k, _)| !matches(k));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ------------------------------------------------------------ 搜索

/// 搜索词：转小写、按空白拆开。
pub fn words(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// 每个词都出现在 `text` 中（不区分大小写）。
pub fn matches_all(words: &[String], text: &str) -> bool {
    if words.is_empty() {
        return true;
    }
    let text = text.to_lowercase();
    words.iter().all(|w| text.contains(w.as_str()))
}

// ------------------------------------------------------------ 进程

/// 进程表可排序的列。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProcSort {
    Name,
    Pid,
    User,
    Mem,
    Cpu,
}

/// 进程表的排序：默认按 CPU 降序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SortOrder {
    pub key: ProcSort,
    pub desc: bool,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self {
            key: ProcSort::Cpu,
            desc: true,
        }
    }
}

impl SortOrder {
    /// 点了某一列：同一列切换升降；新的一列中名称、用户默认升序，其余默认降序。
    pub fn toggled(self, key: ProcSort) -> Self {
        if self.key == key {
            Self {
                key,
                desc: !self.desc,
            }
        } else {
            Self {
                key,
                desc: !matches!(key, ProcSort::Name | ProcSort::User),
            }
        }
    }
}

/// 进程的用户：用户名，没有时为 UID（都没有时为空）。
pub fn process_user(p: &Process) -> String {
    match (&p.user, p.uid) {
        (Some(user), _) => user.clone(),
        (None, Some(uid)) => uid.to_string(),
        (None, None) => String::new(),
    }
}

/// 排序 `rows`（进程在 `list` 中的位置）：名称、用户按字母（不区分大小写），其余按数值，没有 CPU 读数的当作 -1；
/// 相同时按 PID 升序。
pub fn sort_processes(list: &[Process], rows: &mut [usize], order: SortOrder) {
    let cmp = |a: &Process, b: &Process| -> Ordering {
        match order.key {
            ProcSort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            ProcSort::User => process_user(a)
                .to_lowercase()
                .cmp(&process_user(b).to_lowercase()),
            ProcSort::Mem => a.rss.cmp(&b.rss),
            ProcSort::Cpu => a
                .cpu
                .unwrap_or(-1.)
                .partial_cmp(&b.cpu.unwrap_or(-1.))
                .unwrap_or(Ordering::Equal),
            ProcSort::Pid => a.pid.cmp(&b.pid),
        }
    };
    rows.sort_by(|&x, &y| {
        let (a, b) = (&list[x], &list[y]);
        let ordering = cmp(a, b);
        let ordering = if order.desc {
            ordering.reverse()
        } else {
            ordering
        };
        ordering.then_with(|| a.pid.cmp(&b.pid))
    });
}

/// 进程页搜索之外的筛选（显示为可清除的小标签）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcFilter {
    /// 隐藏容器内的进程
    pub hide_containers: bool,
    /// 只列出这个容器内的进程（容器详情的“查看进程”；有它时不看 `hide_containers`）
    pub container: Option<String>,
    /// 只列出这个用户的进程（[`process_user`] 的写法：用户名，没有时为 UID）
    pub user: Option<String>,
}

impl ProcFilter {
    /// 这个进程不被筛选条件藏起来（不看搜索）。
    pub fn matches(&self, p: &Process) -> bool {
        let container = match &self.container {
            Some(id) => p.container.as_ref() == Some(id),
            None => !(self.hide_containers && p.container.is_some()),
        };
        container
            && self
                .user
                .as_ref()
                .is_none_or(|user| process_user(p) == *user)
    }
}

/// 列表中的各个用户与各自的进程数（用户筛选的下拉菜单）：进程多的在前，一样多时按名称；没有用户名也没有 UID 的不算。
pub fn process_users(list: &[Process]) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for p in list {
        let user = process_user(p);
        if !user.is_empty() {
            *counts.entry(user).or_default() += 1;
        }
    }
    let mut users: Vec<(String, usize)> = counts.into_iter().collect();
    users.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| natural_cmp(&a.0, &b.0)));
    users
}

/// 搜索：名称、PID、用户（没有用户名时为 UID）、所属服务，多个词都要命中；再按 `filter` 筛选。返回进程在列表中的位置。
pub fn filter_processes(list: &[Process], query: &str, filter: &ProcFilter) -> Vec<usize> {
    let words = words(query);
    list.iter()
        .enumerate()
        .filter(|(_, p)| filter.matches(p))
        .filter(|(_, p)| {
            let service = p.service.as_deref().map(service_name).unwrap_or_default();
            matches_all(
                &words,
                &format!("{} {} {} {service}", p.name, p.pid, process_user(p)),
            )
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// 合并同名进程后的一组：同名的进程（在列表中的位置，沿用排序后的顺序）与它们的 CPU、内存的合计。
#[derive(Clone, Debug, PartialEq)]
pub struct ProcGroup {
    pub name: String,
    pub members: Vec<usize>,
    /// CPU 的合计；都没有读数时为 `None`
    pub cpu: Option<f64>,
    pub rss: u64,
}

/// 合并同名进程：`rows` 是筛选、排序后的进程，同名的合成一组（只有一个的也是一组）。按 CPU、内存排序时各组按合计
/// 排，其余按组中第一个进程的位置（列表已排好）。
pub fn group_processes(list: &[Process], rows: &[usize], order: SortOrder) -> Vec<ProcGroup> {
    let mut groups: Vec<ProcGroup> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for &ix in rows {
        let p = &list[ix];
        let at = *index.entry(p.name.as_str()).or_insert_with(|| {
            groups.push(ProcGroup {
                name: p.name.clone(),
                members: Vec::new(),
                cpu: None,
                rss: 0,
            });
            groups.len() - 1
        });
        let group = &mut groups[at];
        group.members.push(ix);
        group.rss += p.rss;
        if let Some(cpu) = p.cpu {
            group.cpu = Some(group.cpu.unwrap_or(0.) + cpu);
        }
    }
    let total = |g: &ProcGroup| -> f64 {
        match order.key {
            ProcSort::Cpu => g.cpu.unwrap_or(-1.),
            _ => g.rss as f64,
        }
    };
    if matches!(order.key, ProcSort::Cpu | ProcSort::Mem) {
        // 稳定排序：合计一样时保持原来的先后
        groups.sort_by(|a, b| {
            let ordering = total(a).partial_cmp(&total(b)).unwrap_or(Ordering::Equal);
            if order.desc {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }
    for group in &mut groups {
        group.cpu = group.cpu.map(|cpu| (cpu * 10.).round() / 10.);
    }
    groups
}

/// 进程的标识（PID 与启动时刻）：树形中折叠了哪些进程按它记，PID 被别的进程重新使用后不沿用。
pub type ProcKey = (u32, u64);

pub fn proc_key(p: &Process) -> ProcKey {
    (p.pid, p.start_ticks)
}

/// 各进程的父进程在列表中的位置（树形的上级）。不算父进程的：父进程不在列表中（内核线程、列表之外、已结束）；
/// 父进程比它晚启动（读取的过程中父进程结束、PID 被新的进程重新使用，PPID 指向的已不是它的父进程）；自己是自己的
/// 父进程。读取不是原子的，个别情况下仍可能连成环：环中 PID 最小的不算父进程（成为最上层），不会无限循环。
pub fn process_parents(list: &[Process]) -> Vec<Option<usize>> {
    let by_pid: HashMap<u32, usize> = list.iter().enumerate().map(|(ix, p)| (p.pid, ix)).collect();
    let mut parents: Vec<Option<usize>> = list
        .iter()
        .map(|p| {
            let parent = *by_pid.get(&p.ppid)?;
            let q = &list[parent];
            (q.pid != p.pid && q.start_ticks <= p.start_ticks).then_some(parent)
        })
        .collect();
    // 拆开环：沿父进程往上走，走回这一趟走过的进程就是环
    let mut state = vec![0u8; list.len()]; // 0 没走过，1 这一趟正在走，2 已确定没有环
    for start in 0..list.len() {
        let mut path: Vec<usize> = Vec::new();
        let mut at = Some(start);
        while let Some(ix) = at {
            match state[ix] {
                2 => break,
                1 => {
                    let cycle_from = path.iter().position(|&p| p == ix).unwrap_or(0);
                    let cycle: &[usize] = &path[cycle_from..];
                    if let Some(&root) = cycle.iter().min_by_key(|&&p| list[p].pid) {
                        parents[root] = None;
                    }
                    break;
                }
                _ => {
                    state[ix] = 1;
                    path.push(ix);
                    at = parents[ix];
                }
            }
        }
        for ix in path {
            state[ix] = 2;
        }
    }
    parents
}

/// 树形的一行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRow {
    /// 进程在列表中的位置
    pub ix: usize,
    /// 层级（最上层为 0）
    pub depth: usize,
    /// 树中的上级（进程在列表中的位置）；最上层为 `None`
    pub parent: Option<usize>,
    /// 树中直接的下级数（折叠时它们不显示）
    pub children: usize,
    /// 折叠着
    pub collapsed: bool,
    /// 没有命中搜索与筛选，只是为了看出命中的进程在哪里而列出的上级（界面用次要文字色）
    pub context: bool,
}

/// 树形：`matched` 是命中搜索与筛选的进程，连同它们的各级上级（[`process_parents`]）排成树；同一层内按 `order` 排序，
/// 上级不在其中的在最上层。`collapsed` 中的进程只列出自己，不列出下级。
pub fn process_tree(
    list: &[Process],
    parents: &[Option<usize>],
    matched: &[usize],
    order: SortOrder,
    collapsed: &HashSet<ProcKey>,
) -> Vec<TreeRow> {
    let matched_set: HashSet<usize> = matched.iter().copied().collect();
    let mut shown: HashSet<usize> = HashSet::new();
    for &ix in matched {
        let mut at = Some(ix);
        // 环已在 process_parents 中拆开；遇到已加入的就停（它的上级已经都在）
        while let Some(i) = at {
            if !shown.insert(i) {
                break;
            }
            at = parents.get(i).copied().flatten();
        }
    }
    let mut ordered: Vec<usize> = shown.iter().copied().collect();
    sort_processes(list, &mut ordered, order);
    let mut roots: Vec<usize> = Vec::new();
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for &ix in &ordered {
        match parents
            .get(ix)
            .copied()
            .flatten()
            .filter(|p| shown.contains(p))
        {
            Some(parent) => children.entry(parent).or_default().push(ix),
            None => roots.push(ix),
        }
    }
    let mut rows = Vec::with_capacity(ordered.len());
    // 深度优先，用显式的栈（链很长时不递归）：后进先出，同一层倒着放
    let mut stack: Vec<(usize, usize, Option<usize>)> =
        roots.iter().rev().map(|&ix| (ix, 0, None)).collect();
    while let Some((ix, depth, parent)) = stack.pop() {
        let kids = children.get(&ix).map(Vec::as_slice).unwrap_or(&[]);
        let folded = !kids.is_empty() && collapsed.contains(&proc_key(&list[ix]));
        rows.push(TreeRow {
            ix,
            depth,
            parent,
            children: kids.len(),
            collapsed: folded,
            context: !matched_set.contains(&ix),
        });
        if !folded {
            stack.extend(kids.iter().rev().map(|&kid| (kid, depth + 1, Some(ix))));
        }
    }
    rows
}

/// `ixs` 的各级上级（不含它们自己，除非是别的进程的上级）：搜索、筛选变了时展开它们，命中的进程不被折叠藏起来。
pub fn ancestors(parents: &[Option<usize>], ixs: &[usize]) -> HashSet<usize> {
    let mut out = HashSet::new();
    for &ix in ixs {
        let mut at = parents.get(ix).copied().flatten();
        while let Some(i) = at {
            if !out.insert(i) {
                break;
            }
            at = parents.get(i).copied().flatten();
        }
    }
    out
}

/// 进程标签上的容器名：Docker 页的读数中有这个容器时用它的名称，其次用进程页自己读到的名称（`names`，容器 ID →
/// 名称），都没有时用 ID 的前 12 位。
pub fn container_label(
    id: &str,
    docker: Option<&DockerInfo>,
    names: Option<&HashMap<String, String>>,
) -> String {
    docker
        .and_then(|info| info.containers.iter().find(|c| c.id == id))
        .map(|c| c.name.clone())
        .or_else(|| names.and_then(|names| names.get(id)).cloned())
        .map(|name| visible_name(&name))
        .unwrap_or_else(|| short_id(id).to_string())
}

// ------------------------------------------------------------ 概览

/// 概览页列出的进程数。
pub const TOP_PROCESSES: usize = 5;

/// 概览页显示时另读的部分：系统与内核、各挂载点的 inode（读不到时为 `None`），以及 CPU 占用最高的进程（与进程页是
/// 同一个读取）。CPU、内存、网络、磁盘取自终端自己的资源监控，不在这里。
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewData {
    pub details: Option<Details>,
    /// CPU 占用最高的几个进程，从高到低
    pub top: Vec<Process>,
    /// 第一次读进程列表，还没有 CPU 读数（要两次读数之差）
    pub cpu_pending: bool,
}

impl OverviewData {
    pub fn new(details: Option<Details>, list: &ProcessList) -> Self {
        Self {
            details,
            top: top_by_cpu(&list.processes, TOP_PROCESSES),
            cpu_pending: cpu_pending(list),
        }
    }
}

/// 读到的进程都没有 CPU：第一次读取，或上一次读数太旧、不拿来算（进程页、概览页稍后补读一次）。
pub fn cpu_pending(list: &ProcessList) -> bool {
    !list.processes.is_empty() && list.processes.iter().all(|p| p.cpu.is_none())
}

/// 显示着的进程都还没有 CPU 而 `fresh`（暂停期间补读到的）有时：把 CPU 按进程（PID 与启动时间）就地填进去，
/// 其余不变——进程的顺序、增删都不变，列表不在鼠标下跳动。没有可填的时为 `None`。
pub fn fill_cpu(shown: &ProcessList, fresh: &ProcessList) -> Option<ProcessList> {
    if !cpu_pending(shown) || cpu_pending(fresh) {
        return None;
    }
    let cpu: HashMap<(u32, u64), f64> = fresh
        .processes
        .iter()
        .filter_map(|p| Some(((p.pid, p.start_ticks), p.cpu?)))
        .collect();
    let mut filled = shown.clone();
    for p in &mut filled.processes {
        p.cpu = cpu.get(&(p.pid, p.start_ticks)).copied();
    }
    Some(filled)
}

/// CPU 占用最高的 `n` 个进程（一样时按内存从大到小，再按 PID）；没有 CPU 读数的不算。
pub fn top_by_cpu(list: &[Process], n: usize) -> Vec<Process> {
    let mut top: Vec<&Process> = list.iter().filter(|p| p.cpu.is_some()).collect();
    top.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.rss.cmp(&a.rss))
            .then_with(|| a.pid.cmp(&b.pid))
    });
    top.into_iter().take(n).cloned().collect()
}

/// `pid` 的全部后代（不含自己）。
pub fn descendants(list: &[Process], pid: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in list {
        children.entry(p.ppid).or_default().push(p.pid);
    }
    let mut out = HashSet::new();
    let mut queue: VecDeque<u32> = children.get(&pid).cloned().unwrap_or_default().into();
    while let Some(next) = queue.pop_front() {
        if next == pid || !out.insert(next) {
            continue;
        }
        if let Some(kids) = children.get(&next) {
            queue.extend(kids.iter().copied());
        }
    }
    out
}

/// 结束进程的风险（决定确认框的提示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessRisk {
    /// PID 1：结束后系统或容器通常会停止、重启
    Init,
    /// 这个终端自己的 shell：结束后终端断开
    Shell,
    /// 在这个终端中运行的程序
    InTerminal,
    /// SSH 服务：可能断开连接、无法再登录
    Sshd,
    Normal,
}

impl ProcessRisk {
    /// 测试与元素 ID 用的名称。
    pub fn key(self) -> &'static str {
        match self {
            ProcessRisk::Init => "init",
            ProcessRisk::Shell => "shell",
            ProcessRisk::InTerminal => "self",
            ProcessRisk::Sshd => "sshd",
            ProcessRisk::Normal => "normal",
        }
    }
}

pub fn process_risk(p: &Process) -> ProcessRisk {
    if p.pid == 1 {
        ProcessRisk::Init
    } else if p.terminal_shell {
        ProcessRisk::Shell
    } else if p.in_terminal {
        ProcessRisk::InTerminal
    } else if p.name.starts_with("sshd") || p.name == "dropbear" {
        // sshd、sshd-session（OpenSSH 9.8 起每个连接一个）、dropbear；不含 ssh 客户端
        ProcessRisk::Sshd
    } else {
        ProcessRisk::Normal
    }
}

/// CPU 使用率：保留一位小数，带上 %（单核为 100%），没有读数时为“-”。
pub fn format_cpu(cpu: Option<f64>) -> String {
    cpu.map(|c| format!("{c:.1}%"))
        .unwrap_or_else(|| "-".into())
}

/// 事件的时间（本地时间）：今天的只写时分（`09:47`），别的日子带上日期（今年的 `11-03 09:47`，往年的只写日期）。
pub fn event_time(ms: i64, now: i64, style: DateStyle) -> String {
    let (at, today) = (format_local(ms), format_local(now));
    match (at.get(..10), at.get(11..16)) {
        (Some(day), Some(time)) if Some(day) == today.get(..10) => time.to_string(),
        _ => format_compact(ms, now, style),
    }
}

/// 读取时间的时刻 `HH:MM:SS`（本地时间）。时区的偏移都是整分钟，秒数与时区无关，时分取核心的本地时间格式。
pub fn clock_time(ms: i64) -> String {
    let local = termlet_core::time::format_local(ms);
    let hm = local.get(11..16).unwrap_or_default();
    format!("{hm}:{:02}", ms.div_euclid(1000).rem_euclid(60))
}

// ------------------------------------------------------------ 服务

/// 服务页的子页：服务的筛选（全部、运行中、已停止、失败）与并列的定时器（与 Docker 页的“容器、异常、镜像”相同的做法）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ServiceFilter {
    All,
    Running,
    Stopped,
    Failed,
    /// systemd 定时器（不是服务的筛选，列出的是定时器）
    Timers,
}

impl ServiceFilter {
    pub const ALL: [ServiceFilter; 5] = [
        ServiceFilter::All,
        ServiceFilter::Running,
        ServiceFilter::Stopped,
        ServiceFilter::Failed,
        ServiceFilter::Timers,
    ];
}

/// 运行中：active、reloading、activating。
pub fn is_service_running(s: &Service) -> bool {
    matches!(
        s.active_state.as_str(),
        "active" | "reloading" | "activating"
    )
}

pub fn is_service_failed(s: &Service) -> bool {
    s.active_state == "failed"
}

/// 服务上一次运行的结果（systemd 的原词，管理员认得）：`exit-code 3`、`signal SIGKILL`、`timeout` 等；结果不是
/// 主进程的退出本身时（反复失败后达到启动上限等）括注主进程最后一次怎样结束：`start-limit-hit (exit-code 3)`。
/// 读不到时为 `None`。
pub fn service_result(s: &Service) -> Option<String> {
    let result = s.result.trim();
    if result.is_empty() {
        return None;
    }
    let exit = match s.main_exit {
        Some(MainExit::Code(0)) | None => None,
        Some(MainExit::Code(code)) => Some(("exit-code", code.to_string())),
        Some(MainExit::Signal(signal)) => Some((
            "signal",
            signal_name(signal).map_or_else(|| signal.to_string(), str::to_string),
        )),
    };
    Some(match (result, exit) {
        ("exit-code", Some(("exit-code", code))) => format!("{result} {code}"),
        ("signal" | "core-dump", Some(("signal", signal))) => format!("{result} {signal}"),
        ("success", _) | (_, None) => result.to_string(),
        (_, Some((kind, value))) => format!("{result} ({kind} {value})"),
    })
}

/// 常见信号的名称。
fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// 默认不列出的单元：单元文件不存在的（not-found，多是别的单元提到、却没有装的），以及已停止的静态单元（static，
/// 只由别的单元带起来，自己没有开机启动的说法）。“显示全部”时才列出；失败的照常列出。
pub fn service_hidden_by_default(s: &Service) -> bool {
    !is_service_failed(s)
        && (s.load_state == "not-found"
            || (s.unit_file_state == "static" && !is_service_running(s)))
}

/// 按状态筛选、搜索（服务名与描述，多个词都要命中）；`show_all` 为假时不列出 [`service_hidden_by_default`] 的单元。
/// 返回服务在列表中的位置。
pub fn filter_services(
    list: &[Service],
    filter: ServiceFilter,
    show_all: bool,
    query: &str,
) -> Vec<usize> {
    let words = words(query);
    list.iter()
        .enumerate()
        .filter(|(_, s)| show_all || !service_hidden_by_default(s))
        .filter(|(_, s)| match filter {
            ServiceFilter::All => true,
            ServiceFilter::Running => is_service_running(s),
            ServiceFilter::Failed => is_service_failed(s),
            ServiceFilter::Stopped => !is_service_running(s) && !is_service_failed(s),
            ServiceFilter::Timers => false,
        })
        .filter(|(_, s)| matches_all(&words, &format!("{} {}", s.unit, s.description)))
        .map(|(ix, _)| ix)
        .collect()
}

/// 服务的操作，按界面上的顺序。
pub const SERVICE_OPS: [ServiceOp; 6] = [
    ServiceOp::Start,
    ServiceOp::Stop,
    ServiceOp::Restart,
    ServiceOp::Reload,
    ServiceOp::Enable,
    ServiceOp::Disable,
];

/// 操作是否适用于当前状态：运行中的不再启动，已停止的不能停止、重新加载；启用、禁用开机启动看单元文件的状态。
pub fn service_op_applies(s: &Service, op: ServiceOp) -> bool {
    let running = is_service_running(s);
    match op {
        ServiceOp::Start => !running,
        ServiceOp::Stop | ServiceOp::Reload => running,
        ServiceOp::Restart => true,
        ServiceOp::Enable => s.unit_file_state == "disabled",
        ServiceOp::Disable => s.unit_file_state == "enabled",
    }
}

/// 服务操作的确认分级（都先确认）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceRisk {
    /// 一般确认：写明影响
    Confirm,
    /// SSH、网络与防火墙类服务：可能断开连接、无法再登录
    Connection,
    /// 关机、重启、休眠类单元：主机会立即关机、重启或休眠
    Power,
}

impl ServiceRisk {
    pub fn key(self) -> &'static str {
        match self {
            ServiceRisk::Confirm => "confirm",
            ServiceRisk::Connection => "connection",
            ServiceRisk::Power => "power",
        }
    }
}

/// 操作后可能断开连接、无法再登录的服务（启动、重新加载防火墙也可能把自己挡在外面）。
static CONNECTION_SERVICES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(ssh|sshd|openssh|openssh-server|dropbear|NetworkManager|network|networking|systemd-networkd|systemd-resolved|firewalld|iptables|ip6tables|nftables|ufw|wicked|netplan-.*|ifup@.*|wpa_supplicant|tailscaled|wg-quick@.*|openvpn.*|strongswan.*)\.service$",
    )
    .expect("正则表达式")
});

/// 关机、重启、休眠类单元：启动它们就是让主机关机、重启、挂起（systemctl poweroff 等实际启动的就是它们）。
static POWER_SERVICES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^systemd-(poweroff|reboot|halt|kexec|soft-reboot|exit|suspend|hibernate|hybrid-sleep|suspend-then-hibernate)\.service$",
    )
    .expect("正则表达式")
});

pub fn service_risk(unit: &str, op: ServiceOp) -> ServiceRisk {
    if POWER_SERVICES.is_match(unit)
        && matches!(
            op,
            ServiceOp::Start | ServiceOp::Restart | ServiceOp::Reload
        )
    {
        ServiceRisk::Power
    } else if CONNECTION_SERVICES.is_match(unit) && op != ServiceOp::Enable {
        ServiceRisk::Connection
    } else {
        ServiceRisk::Confirm
    }
}

/// 信息行上的 systemd 版本：`systemctl --version` 第一行的前两个词（`systemd 257 (257.9-1~deb13u1)` →
/// `systemd 257`），不截断整串。
pub fn systemd_version(line: &str) -> String {
    line.split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// 服务列表的分组：有自定义的服务时，自定义的在前、系统的在后，各有一行标题（写着数量）；都是系统服务时不分组。
/// `rows` 是筛选后的服务（核心已把自定义的排在前面）。定时器子页每个定时器一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceRow {
    /// 分组的标题：自定义服务（`custom`）或系统服务，与这一组的数量
    Header { custom: bool, count: usize },
    /// 一个服务（在列表中的位置）
    Service(usize),
    /// 一个定时器（在定时器列表中的位置）
    Timer(usize),
}

/// 定时器的操作，按界面上的顺序（重启、重新加载对定时器没有意义）。
pub const TIMER_OPS: [ServiceOp; 4] = [
    ServiceOp::Start,
    ServiceOp::Stop,
    ServiceOp::Enable,
    ServiceOp::Disable,
];

/// 定时器在计时（active：等待中、触发中、已触发过的一次性定时器）。
pub fn is_timer_active(t: &Timer) -> bool {
    t.active_state == "active"
}

/// 定时器的操作是否适用：在计时的不再启动，没在计时的不能停止；开机启用、禁用看单元文件的状态。
pub fn timer_op_applies(t: &Timer, op: ServiceOp) -> bool {
    match op {
        ServiceOp::Start => !is_timer_active(t),
        ServiceOp::Stop => is_timer_active(t),
        ServiceOp::Enable => t.unit_file_state == "disabled",
        ServiceOp::Disable => t.unit_file_state == "enabled",
        ServiceOp::Restart | ServiceOp::Reload => false,
    }
}

/// 搜索定时器：定时器名、描述与触发的单元，多个词都要命中。返回定时器在列表中的位置（沿用核心的顺序：下次触发早的在前）。
pub fn filter_timers(list: &[Timer], query: &str) -> Vec<usize> {
    let words = words(query);
    list.iter()
        .enumerate()
        .filter(|(_, t)| {
            matches_all(
                &words,
                &format!("{} {} {}", t.unit, t.description, t.activates),
            )
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// 触发这个服务的定时器（服务详情中写出，可以跳过去）。
pub fn timers_of<'a>(timers: &'a [Timer], unit: &str) -> Vec<&'a Timer> {
    timers.iter().filter(|t| t.activates == unit).collect()
}

/// 定时器名去掉 `.timer`（界面显示）。
pub fn timer_name(unit: &str) -> &str {
    unit.strip_suffix(".timer").unwrap_or(unit)
}

// ------------------------------------------------------------ 定时器的时间与时区

/// 偏移写成 `UTC+8`、`UTC+5:45`、`UTC-5`、`UTC`。
pub fn utc_offset_label(offset: i32) -> String {
    if offset == 0 {
        return "UTC".into();
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    match (minutes / 60, minutes % 60) {
        (h, 0) => format!("UTC{sign}{h}"),
        (h, m) => format!("UTC{sign}{h}:{m:02}"),
    }
}

/// 服务器时区的名称：时区名（`Asia/Shanghai`），没有时用字母的缩写（`CST`；`+08` 这样的数字缩写不用）；都没有时为空。
pub fn zone_name(zone: &ServerZone) -> &str {
    if !zone.name.is_empty() {
        &zone.name
    } else if zone.abbr.chars().any(|c| c.is_ascii_alphabetic()) {
        &zone.abbr
    } else {
        ""
    }
}

/// 日历表达式写明了时区（`*-*-* 03:00:00 Europe/Berlin`、`… UTC`）：这时不按服务器的时区解释。
pub fn calendar_has_zone(expression: &str) -> bool {
    expression
        .split_whitespace()
        .last()
        .is_some_and(|last| last.contains('/') || matches!(last, "UTC" | "GMT" | "Z"))
}

/// 事件的时间按偏移为 `offset`（秒）的时区（定时器按服务器那时的偏移）：那个时区的今天只写时分（`09:47`），别的日子
/// 带上日期；不知道偏移时按本地时间（[`event_time`]）。
pub fn event_time_at(ms: i64, now: i64, style: DateStyle, offset: Option<i32>) -> String {
    use termlet_core::time::{clock_at, format_compact_at, same_day_at};
    match offset {
        Some(offset) if same_day_at(ms, now, offset) => clock_at(ms, offset),
        Some(offset) => format_compact_at(ms, now, style, offset),
        None => event_time(ms, now, style),
    }
}

/// 定时器的一个时刻按服务器的时间显示所需的：服务器那时的偏移（不知道时为 `None`，只能按本机显示），与本机是否不同
/// （不同时另写本机的时间）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerClock {
    pub offset: Option<i32>,
    pub differs: bool,
}

impl TimerClock {
    /// `offset` 是服务器在 `ms` 那一刻的偏移（[`Timer::next_offset`] 等）。
    pub fn at(ms: i64, offset: Option<i32>) -> Self {
        let differs = match (offset, termlet_core::time::local_offset(ms)) {
            (Some(server), Some(local)) => server != local,
            _ => false,
        };
        Self { offset, differs }
    }
}

pub fn service_rows(list: &[Service], rows: &[usize]) -> Vec<ServiceRow> {
    let (custom, system): (Vec<usize>, Vec<usize>) = rows.iter().partition(|&&ix| list[ix].custom);
    if custom.is_empty() {
        return system.into_iter().map(ServiceRow::Service).collect();
    }
    let mut out = vec![ServiceRow::Header {
        custom: true,
        count: custom.len(),
    }];
    out.extend(custom.into_iter().map(ServiceRow::Service));
    if !system.is_empty() {
        out.push(ServiceRow::Header {
            custom: false,
            count: system.len(),
        });
        out.extend(system.into_iter().map(ServiceRow::Service));
    }
    out
}

/// 服务名去掉 `.service`（界面显示、新 Tab 的标题）。
pub fn service_name(unit: &str) -> &str {
    unit.strip_suffix(".service").unwrap_or(unit)
}

// ------------------------------------------------------------ Docker

/// 一个 compose 项目（或单独的容器）的容器。
pub struct ContainerGroup<'a> {
    /// compose 项目名；单独的容器为 `None`
    pub project: Option<&'a str>,
    pub working_dir: Option<&'a str>,
    pub containers: Vec<&'a Container>,
    pub running: usize,
}

/// running、restarting、paused 都算运行中。
pub fn is_container_running(c: &Container) -> bool {
    matches!(c.state.as_str(), "running" | "restarting" | "paused")
}

/// 容器的异常：与正常退出（退出码 0）、手动停止区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerProblem {
    /// 以非 0 的退出码退出（警告色）
    Exited(i64),
    /// 健康检查失败
    Unhealthy,
    /// 重启中（多半是启动后又退出，反复重启）
    Restarting,
    /// 已失效（dead：删除失败等）
    Dead,
}

impl ContainerProblem {
    /// 用危险色（不健康、重启中、已失效）；非 0 退出用警告色。
    pub fn is_danger(self) -> bool {
        !matches!(self, ContainerProblem::Exited(_))
    }
}

pub fn container_problem(c: &Container) -> Option<ContainerProblem> {
    match c.state.as_str() {
        "dead" => Some(ContainerProblem::Dead),
        "restarting" => Some(ContainerProblem::Restarting),
        _ if c.health() == Some(HealthState::Unhealthy) => Some(ContainerProblem::Unhealthy),
        "exited" => c
            .exit_code()
            .filter(|&code| code != 0)
            .map(ContainerProblem::Exited),
        _ => None,
    }
}

/// 容器列表的排序。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ContainerSort {
    /// 按 compose 项目与名称（核心的顺序）
    #[default]
    Project,
    /// 按 CPU 使用率从高到低
    Cpu,
    /// 按内存用量从高到低
    Memory,
}

impl ContainerSort {
    pub const ALL: [ContainerSort; 3] = [
        ContainerSort::Project,
        ContainerSort::Cpu,
        ContainerSort::Memory,
    ];
}

/// 按 compose 项目分组：项目按名称排序，单独的容器放在最后，组内沿用核心的顺序；可按名称、镜像、项目、服务搜索，
/// `problems_only` 时只留有异常的。给出 `key` 时（按 CPU、内存排序）组内按它从高到低，各组按组内最高的从高到低
/// （没有读数的在后，同样多时保持原来的顺序）。
pub fn container_groups<'a>(
    list: &'a [Container],
    query: &str,
    problems_only: bool,
    key: Option<&dyn Fn(&Container) -> Option<f64>>,
) -> Vec<ContainerGroup<'a>> {
    let words = words(query);
    let mut groups: Vec<ContainerGroup<'a>> = Vec::new();
    for c in list {
        if problems_only && container_problem(c).is_none() {
            continue;
        }
        let text = format!(
            "{} {} {} {}",
            c.name,
            c.image,
            c.project.as_deref().unwrap_or_default(),
            c.service.as_deref().unwrap_or_default()
        );
        if !matches_all(&words, &text) {
            continue;
        }
        let project = c.project.as_deref();
        let ix = match groups.iter().position(|g| g.project == project) {
            Some(ix) => ix,
            None => {
                groups.push(ContainerGroup {
                    project,
                    working_dir: None,
                    containers: Vec::new(),
                    running: 0,
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[ix];
        group.containers.push(c);
        if is_container_running(c) {
            group.running += 1;
        }
        if group.working_dir.is_none() {
            group.working_dir = c.working_dir.as_deref();
        }
    }
    groups.sort_by(|a, b| match (a.project, b.project) {
        (Some(x), Some(y)) => natural_cmp(x, y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    });
    if let Some(key) = key {
        // 从高到低，没有读数的在后
        let desc = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => b.total_cmp(&a),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        for group in &mut groups {
            group.containers.sort_by(|a, b| desc(key(a), key(b)));
        }
        let top = |g: &ContainerGroup| g.containers.first().and_then(|c| key(c));
        groups.sort_by(|a, b| desc(top(a), top(b)));
    }
    groups
}

/// 列表中端口映射的精简写法：全部地址上的只写端口，IPv4 与 IPv6 的同一个只写一次，tcp 不写协议，没有发布的只写
/// 容器端口。`0.0.0.0:8081->80/tcp, :::8081->80/tcp, 6379/tcp, 127.0.0.1:5353->53/udp` →
/// `8081→80, 6379, 127.0.0.1:5353→53/udp`。
pub fn compact_ports(ports: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for item in ports.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let short_proto = |p: &str| p.strip_suffix("/tcp").unwrap_or(p).to_string();
        let entry = match item.split_once("->") {
            Some((host, container)) => {
                let (addr, port) = host.rsplit_once(':').unwrap_or(("", host));
                let any = matches!(addr, "" | "0.0.0.0" | "::" | "[::]" | ":");
                let host = if any {
                    port.to_string()
                } else {
                    format!("{addr}:{port}")
                };
                format!("{host}→{}", short_proto(container))
            }
            None => short_proto(item),
        };
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out.join(", ")
}

/// 一个端口范围最多展开这么多个端口（`8000-9000->8000-9000/tcp` 这样的大范围只认前面的）。
const MAX_RANGE: u32 = 1024;

/// 容器发布到主机的端口（`docker ps` 的 Ports：`0.0.0.0:8081->80/tcp, :::8081->80/tcp`，范围写作
/// `0.0.0.0:8000-8001->8000-8001/tcp`）：（协议, 主机端口），去重。没有发布的（`6379/tcp`）不算。
pub fn published_host_ports(ports: &str) -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = Vec::new();
    for item in ports.split(',').map(str::trim) {
        let Some((host, container)) = item.split_once("->") else {
            continue;
        };
        let proto = container.rsplit_once('/').map_or("tcp", |(_, p)| p);
        let port = host.rsplit_once(':').map_or(host, |(_, p)| p);
        let (first, last) = match port.split_once('-') {
            Some((a, b)) => (a.parse::<u16>(), b.parse::<u16>()),
            None => (port.parse::<u16>(), port.parse::<u16>()),
        };
        let (Ok(first), Ok(last)) = (first, last) else {
            continue;
        };
        let last = u32::from(last).min(u32::from(first) + MAX_RANGE - 1);
        for port in u32::from(first)..=last {
            let entry = (proto.to_string(), port as u16);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out
}

/// 各容器发布的端口：（协议, 主机端口）→ 容器在列表中的位置（同一个端口只认第一个，运行中的在前）。
pub fn port_owners(containers: &[Container]) -> HashMap<(String, u16), usize> {
    let mut map = HashMap::new();
    let running = containers
        .iter()
        .enumerate()
        .filter(|(_, c)| is_container_running(c));
    let stopped = containers
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_container_running(c));
    for (ix, c) in running.chain(stopped) {
        for key in published_host_ports(&c.ports) {
            map.entry(key).or_insert(ix);
        }
    }
    map
}

/// Docker、rootless 的 Docker / Podman 转发发布端口的进程：监听着的端口由它们打开时，是某个容器发布的端口。
pub fn is_port_proxy(name: &str) -> bool {
    matches!(name, "docker-proxy" | "rootlessport" | "rootlessport-chi")
}

/// 详情中的一个端口：`8081 → 80/tcp`（全部地址）、`127.0.0.1:5353 → 53/udp`；没有发布的为 `None`。
pub fn published_port(p: &PortBinding) -> Option<String> {
    let host_port = p.host_port?;
    let host = match &p.host_ip {
        Some(ip) => host_port_text(ip, host_port),
        None => host_port.to_string(),
    };
    Some(format!("{host} → {}/{}", p.container_port, p.proto))
}

/// 地址:端口（IPv6 加方括号）。
fn host_port_text(addr: &str, port: u16) -> String {
    if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    }
}

/// 发布到主机的 TCP 端口可以“转发到本机”：经由主机访问它监听的地址（全部地址时为 127.0.0.1），本机也用这个端口，
/// 名称为容器名。
pub fn container_forward_preset(p: &PortBinding, name: &str) -> Option<ForwardPreset> {
    let port = p.host_port.filter(|_| p.proto == "tcp")?;
    let target_host = match p.host_ip.as_deref() {
        None | Some("::") | Some("0.0.0.0") => "127.0.0.1".to_string(),
        Some(ip) => ip.to_string(),
    };
    Some(ForwardPreset {
        target_host,
        port,
        name: name.to_string(),
    })
}

/// 镜像、卷、网络能否删除：没有容器在用，且不是内置的网络。
pub fn image_removable(im: &Image) -> bool {
    !im.in_use
}

pub fn volume_removable(v: &Volume) -> bool {
    !v.in_use
}

pub fn network_removable(n: &Network) -> bool {
    !n.in_use && !n.builtin
}

/// ID 的前 12 位（去掉 `sha256:`）。
pub fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.get(..12).unwrap_or(id)
}

/// 镜像的显示名：仓库:标签；悬空镜像（`<none>`）显示 ID 的开头。
pub fn image_name(im: &Image) -> String {
    if im.repository.is_empty() || im.repository == "<none>" {
        short_id(&im.id).to_string()
    } else {
        format!("{}:{}", im.repository, im.tag)
    }
}

/// 镜像、卷、网络的搜索：整个查询当作一个子串（不拆词），任一字段包含即可。
pub fn matches_any(query: &str, fields: &[&str]) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(&query))
}

// ------------------------------------------------------------ 网络

/// 网络页的筛选。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConnFilter {
    All,
    Listen,
    Established,
    Tcp,
    Udp,
}

impl ConnFilter {
    pub const ALL: [ConnFilter; 5] = [
        ConnFilter::All,
        ConnFilter::Listen,
        ConnFilter::Established,
        ConnFilter::Tcp,
        ConnFilter::Udp,
    ];
}

/// 监听：TCP LISTEN 与 UDP 未连接（UNCONN）。
pub fn is_listening(proto: &str, state: &str) -> bool {
    state == "LISTEN" || (proto == "udp" && state == "UNCONN")
}

/// 网络页搜索之外的筛选条件（从别的页跳过来时加上，显示为可清除的小标签）。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConnChip {
    /// 这个进程的连接（进程详情、服务的主进程）；`name` 是标签上显示的进程名（不知道时为 `None`）
    Pid { pid: u32, name: Option<String> },
    /// 本机这个端口上的连接（容器发布的端口）
    Port(u16),
}

impl ConnChip {
    pub fn matches(&self, c: &Conn) -> bool {
        match self {
            ConnChip::Pid { pid, .. } => c.processes.iter().any(|p| p.pid == *pid),
            ConnChip::Port(port) => c.local_port == *port,
        }
    }
}

/// 按筛选、筛选条件与搜索（协议、状态、本地与对端地址、各进程的名称与 PID、用户）。返回连接在列表中的位置。
pub fn filter_conns(
    list: &[Conn],
    filter: ConnFilter,
    chip: Option<&ConnChip>,
    query: &str,
) -> Vec<usize> {
    let words = words(query);
    list.iter()
        .enumerate()
        .filter(|(_, c)| chip.is_none_or(|chip| chip.matches(c)))
        .filter(|(_, c)| match filter {
            ConnFilter::All => true,
            ConnFilter::Listen => is_listening(&c.proto, &c.state),
            ConnFilter::Established => c.state == "ESTAB",
            ConnFilter::Tcp => c.proto == "tcp",
            ConnFilter::Udp => c.proto == "udp",
        })
        .filter(|(_, c)| {
            let procs: Vec<String> = c
                .processes
                .iter()
                .map(|p| format!("{} {}", p.name, p.pid))
                .collect();
            let text = format!(
                "{} {} {} {} {} {}",
                c.proto,
                c.state,
                host_port(&c.local_addr, c.local_port),
                host_port(&c.peer_addr, c.peer_port),
                procs.join(" "),
                c.user.as_deref().unwrap_or_default()
            );
            matches_all(&words, &text)
        })
        .map(|(ix, _)| ix)
        .collect()
}

/// 顶部的汇总：总数、监听、已连接、TIME-WAIT 与按协议的总数（来自各状态的总数，不受列出上限的影响）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ConnSummary {
    pub total: usize,
    pub listen: usize,
    pub established: usize,
    pub time_wait: usize,
    pub tcp: usize,
    pub udp: usize,
}

pub fn conn_summary(list: &ConnList) -> ConnSummary {
    let mut out = ConnSummary {
        total: list.total,
        ..Default::default()
    };
    for c in &list.counts {
        if is_listening(&c.proto, &c.state) {
            out.listen += c.count;
        }
        match c.state.as_str() {
            "ESTAB" => out.established += c.count,
            "TIME-WAIT" => out.time_wait += c.count,
            _ => {}
        }
        match c.proto.as_str() {
            "tcp" => out.tcp += c.count,
            "udp" => out.udp += c.count,
            _ => {}
        }
    }
    out
}

/// 地址:端口（IPv6 加方括号；端口为 0 时写 `*`）。
pub fn host_port(addr: &str, port: u16) -> String {
    let port = if port == 0 {
        "*".to_string()
    } else {
        port.to_string()
    };
    if addr.contains(':') {
        format!("[{addr}]:{port}")
    } else {
        format!("{addr}:{port}")
    }
}

/// 监听中的 TCP 端口可以“转发到本机”（还要求主机是保存的 SSH 主机）。
pub fn can_forward(c: &Conn) -> bool {
    c.proto == "tcp" && c.state == "LISTEN"
}

/// “转发到本机…”的预填。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardPreset {
    /// 服务器上的目标地址：监听全部地址时经 127.0.0.1 访问
    pub target_host: String,
    /// 目标端口与本机端口都用监听的端口
    pub port: u16,
    /// 名称：第一个进程名，没有时为空
    pub name: String,
}

/// 常见的 HTTP 端口：监听着它们的行上直接有“在浏览器中打开”的按钮（其余的 TCP 监听端口在右键菜单中也能打开，
/// 按 http）。
pub const HTTP_PORTS: [u16; 20] = [
    80, 81, 3000, 3001, 4000, 4200, 5000, 5173, 5601, 7860, 8000, 8001, 8008, 8080, 8081, 8088,
    8501, 8888, 9000, 9090,
];
/// 常见的 HTTPS 端口：在浏览器中打开时用 https。
pub const HTTPS_PORTS: [u16; 4] = [443, 4443, 8443, 9443];

/// 看起来是网页的端口（[`HTTP_PORTS`]、[`HTTPS_PORTS`]）。
pub fn is_web_port(port: u16) -> bool {
    HTTP_PORTS.contains(&port) || HTTPS_PORTS.contains(&port)
}

/// “在浏览器中打开”：转发的预填（与“转发到本机…”相同的目标与名称）与是否用 https（按端口猜）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserPreset {
    pub forward: ForwardPreset,
    pub https: bool,
}

impl BrowserPreset {
    pub fn new(forward: ForwardPreset) -> Self {
        let https = HTTPS_PORTS.contains(&forward.port);
        Self { forward, https }
    }
}

pub fn forward_preset(c: &Conn) -> ForwardPreset {
    let any = matches!(c.local_addr.as_str(), "*" | "0.0.0.0" | "::");
    ForwardPreset {
        target_host: if any {
            "127.0.0.1".into()
        } else {
            c.local_addr.clone()
        },
        port: c.local_port,
        name: c
            .processes
            .first()
            .map(|p| p.name.clone())
            .unwrap_or_default(),
    }
}

/// 每个连接的标识（元素 ID）：协议与两端的地址；同一个套接字出现多次（多个进程共用）时按出现的先后加序号。
pub fn conn_keys(list: &[Conn]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    list.iter()
        .map(|c| {
            let base = format!(
                "{}-{}-{}",
                c.proto,
                host_port(&c.local_addr, c.local_port),
                host_port(&c.peer_addr, c.peer_port)
            );
            let n = seen.entry(base.clone()).or_default();
            *n += 1;
            if *n == 1 { base } else { format!("{base}#{n}") }
        })
        .collect()
}

/// 监听视图的一行：同一协议、同一端口的各个地址（IPv4 与 IPv6）合成一行。
#[derive(Clone, Debug, PartialEq)]
pub struct ListenRow {
    pub proto: String,
    pub port: u16,
    /// 监听的地址（去重，按出现的先后）
    pub addrs: Vec<String>,
    /// 使用它的进程（去重）
    pub processes: Vec<ConnProcess>,
    /// 套接字所属的用户（没有进程时显示）
    pub user: Option<String>,
    /// 元素 ID：`协议-端口`
    pub key: String,
}

impl ListenRow {
    /// 用于“转发到本机…”的那个连接：TCP，优先监听全部地址的。
    pub fn forward_conn<'a>(&self, list: &'a [Conn], rows: &[usize]) -> Option<&'a Conn> {
        let mut found = rows
            .iter()
            .map(|&ix| &list[ix])
            .filter(|c| c.proto == self.proto && c.local_port == self.port && can_forward(c));
        let first = found.clone().next();
        found
            .find(|c| matches!(c.local_addr.as_str(), "*" | "0.0.0.0" | "::"))
            .or(first)
    }
}

/// 监听视图的行：`rows` 是筛选后的连接（已经只有监听着的），按端口、协议排序后同一协议、同一端口的合成一行。
pub fn listen_rows(list: &[Conn], rows: &[usize]) -> Vec<ListenRow> {
    let mut out: Vec<ListenRow> = Vec::new();
    let mut index: HashMap<(String, u16), usize> = HashMap::new();
    for &ix in rows {
        let c = &list[ix];
        let at = *index
            .entry((c.proto.clone(), c.local_port))
            .or_insert_with(|| {
                out.push(ListenRow {
                    proto: c.proto.clone(),
                    port: c.local_port,
                    addrs: Vec::new(),
                    processes: Vec::new(),
                    user: None,
                    key: format!("{}-{}", c.proto, c.local_port),
                });
                out.len() - 1
            });
        let row = &mut out[at];
        if !row.addrs.contains(&c.local_addr) {
            row.addrs.push(c.local_addr.clone());
        }
        for p in &c.processes {
            if !row.processes.iter().any(|q| q.pid == p.pid) {
                row.processes.push(p.clone());
            }
        }
        if row.user.is_none() {
            row.user = c.user.clone();
        }
    }
    out.sort_by(|a, b| a.port.cmp(&b.port).then_with(|| a.proto.cmp(&b.proto)));
    out
}

/// 监听范围的一部分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindPart {
    /// 全部地址：IPv4 与 IPv6 都监听任意地址，或者 ss 的双栈写法 `*`
    Any,
    /// 全部 IPv4 地址（`0.0.0.0`）
    AnyV4,
    /// 全部 IPv6 地址（`::`；系统允许时也接受 IPv4）
    AnyV6,
    /// 一个具体的地址（`127.0.0.1`、`::1`、`10.0.0.5`）
    Addr(String),
}

/// 监听的范围：任意地址的合成一项（IPv4 与 IPv6 都有时为“全部地址”），其余地址照写。
pub fn bind_parts(addrs: &[String]) -> Vec<BindPart> {
    let has = |a: &str| addrs.iter().any(|x| x == a);
    let (v4, v6) = (has("0.0.0.0"), has("::"));
    let mut parts = if has("*") || (v4 && v6) {
        vec![BindPart::Any]
    } else if v4 {
        vec![BindPart::AnyV4]
    } else if v6 {
        vec![BindPart::AnyV6]
    } else {
        Vec::new()
    };
    parts.extend(
        addrs
            .iter()
            .filter(|a| !matches!(a.as_str(), "*" | "0.0.0.0" | "::"))
            .map(|a| BindPart::Addr(a.clone())),
    );
    parts
}

#[cfg(test)]
mod tests {
    use termlet_core::hosttools::{ConnCount, ConnSource};

    use super::*;

    fn proc(pid: u32) -> Process {
        Process {
            pid,
            ppid: 1,
            name: format!("p{pid}"),
            state: 'S',
            uid: Some(1000),
            user: Some("termlet".into()),
            rss: 0,
            vsize: 0,
            cpu: None,
            threads: 1,
            priority: 20,
            nice: 0,
            start: None,
            start_ticks: 0,
            cpu_time: 0.,
            in_terminal: false,
            terminal_shell: false,
            service: None,
            container: None,
            cmdline: None,
        }
    }

    /// 概览页的进程：按 CPU 取最高的几个（一样时按内存、再按 PID），没有 CPU 读数的不算；第一次读取全都没有时标出
    /// “正在计算”。
    #[test]
    fn top_processes_by_cpu() {
        let p = |pid: u32, cpu: Option<f64>, rss: u64| Process {
            cpu,
            rss,
            ..proc(pid)
        };
        let list = [
            p(1, Some(0.), 0),
            p(2, Some(50.), 10),
            p(3, None, 999),
            p(4, Some(50.), 20),
            p(5, Some(99.5), 0),
            p(6, Some(1.), 0),
            p(7, Some(1.), 0),
        ];
        let pids: Vec<u32> = top_by_cpu(&list, 5).iter().map(|p| p.pid).collect();
        assert_eq!(pids, [5, 4, 2, 6, 7]);
        let processes = |list: Vec<Process>| ProcessList {
            total: list.len(),
            processes: list,
            truncated: false,
            uid: None,
            mem_total: 0,
            cores: 1,
            at: 0,
        };
        let first = OverviewData::new(None, &processes(vec![proc(1), proc(2)]));
        assert!(first.cpu_pending && first.top.is_empty());
        let next = OverviewData::new(None, &processes(list.to_vec()));
        assert!(!next.cpu_pending);
        assert_eq!(next.top.len(), TOP_PROCESSES);
        assert!(!OverviewData::new(None, &processes(Vec::new())).cpu_pending);
    }

    /// 暂停期间补读到的 CPU 按 PID 与启动时间就地填进显示着的行：顺序、增删都不变；PID 被重用的、补读中没有的
    /// 仍没有读数；显示着的已有 CPU、补读的也没有时不填。
    #[test]
    fn paused_cpu_retry_fills_the_shown_rows_in_place() {
        let processes = |list: Vec<Process>| ProcessList {
            total: list.len(),
            processes: list,
            truncated: false,
            uid: None,
            mem_total: 0,
            cores: 1,
            at: 0,
        };
        let with_cpu = |pid: u32, cpu: f64| Process {
            cpu: Some(cpu),
            ..proc(pid)
        };
        let shown = processes(vec![proc(3), proc(1), proc(2), proc(4)]);
        let reused = Process {
            start_ticks: 9,
            ..with_cpu(2, 50.)
        };
        let fresh = processes(vec![
            with_cpu(1, 10.),
            reused,
            with_cpu(3, 30.),
            with_cpu(5, 5.),
        ]);
        let filled = fill_cpu(&shown, &fresh).expect("可填");
        let pids: Vec<u32> = filled.processes.iter().map(|p| p.pid).collect();
        assert_eq!(pids, [3, 1, 2, 4], "顺序与行不变");
        let cpu: Vec<Option<f64>> = filled.processes.iter().map(|p| p.cpu).collect();
        assert_eq!(cpu, [Some(30.), Some(10.), None, None]);
        assert!(fill_cpu(&filled, &fresh).is_none(), "显示着的已有 CPU");
        assert!(fill_cpu(&shown, &shown).is_none(), "补读的也没有 CPU");
    }

    #[test]
    fn width_is_clamped_and_leaves_room_on_the_left() {
        assert_eq!(clamp_width(100., 1600., WIDTH_MIN, 1.), WIDTH_MIN);
        assert_eq!(clamp_width(9999., 1600., WIDTH_MIN, 1.), WIDTH_MAX);
        assert_eq!(clamp_width(9999., 1000., WIDTH_MIN, 1.), 1000. - REST_MIN);
        assert_eq!(clamp_width(9999., 600., WIDTH_MIN, 1.), WIDTH_MIN);
        assert_eq!(clamp_width(400.4, 1600., WIDTH_MIN, 1.), 400.);
        // 页签放不下时最小宽度更宽，窗口窄时也以它为准
        assert_eq!(clamp_width(100., 1600., 360., 1.), 360.);
        assert_eq!(clamp_width(9999., 600., 360., 1.), 360.);
        // 界面放大到 150%：最大宽度与给左侧留的宽度一起放大
        assert_eq!(
            clamp_width(9999., 2400., WIDTH_MIN * 1.5, 1.5),
            WIDTH_MAX * 1.5
        );
        assert_eq!(
            clamp_width(9999., 1600., WIDTH_MIN * 1.5, 1.5),
            1600. - REST_MIN * 1.5
        );
        assert_eq!(
            clamp_width(9999., 1000., WIDTH_MIN * 1.5, 1.5),
            WIDTH_MIN * 1.5
        );
    }

    #[test]
    fn saved_width_and_page_fall_back_to_defaults() {
        assert_eq!(saved_width(None), WIDTH_DEFAULT);
        assert_eq!(saved_width(Some(500.)), 500.);
        assert_eq!(saved_width(Some(99999.)), WIDTH_DEFAULT);
        assert_eq!(saved_width(Some(f32::NAN)), WIDTH_DEFAULT);
        assert_eq!(Page::from_saved(None), Page::Overview, "第一页是概览");
        assert_eq!(Page::from_saved(Some("docker")), Page::Docker);
        assert_eq!(Page::from_saved(Some("files")), Page::Overview);
        assert_eq!(Page::ALL[0], Page::Overview);
        for page in Page::ALL {
            assert_eq!(Page::from_saved(Some(page.key())), page);
        }
    }

    #[test]
    fn paused_results_wait_and_updated_at_follows_the_shown_data() {
        let mut s = Shown::new(Some(Rc::new("a")));
        assert_eq!(s.at, None, "缓存不显示“更新于”");
        s.arrive(Rc::new("b"), 2, true, false);
        assert_eq!(*s.data.clone().unwrap(), "a");
        assert_eq!(s.at, None);
        s.arrive(Rc::new("c"), 3, true, false);
        s.resume();
        assert_eq!(*s.data.clone().unwrap(), "c");
        assert_eq!(s.at, Some(3));
        s.resume();
        assert_eq!(s.at, Some(3), "没有暂存时不变");
    }

    #[test]
    fn forced_reload_shows_directly_and_drops_pending() {
        let mut s = Shown::new(Some(Rc::new("a")));
        s.arrive(Rc::new("b"), 2, true, false);
        s.arrive(Rc::new("c"), 3, true, true);
        assert_eq!(*s.data.clone().unwrap(), "c");
        s.resume();
        assert_eq!(*s.data.clone().unwrap(), "c");
    }

    #[test]
    fn cache_keeps_the_most_recent_and_forgets_a_terminal() {
        let mut cache: Lru<(u32, Page), u32> = Lru::new(3);
        cache.put((1, Page::Processes), 10);
        cache.put((1, Page::Docker), 11);
        cache.put((2, Page::Processes), 20);
        // 再记一次第一项：移到最后，最久没用的变成 (1, Docker)
        cache.put((1, Page::Processes), 12);
        cache.put((3, Page::Processes), 30);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get(&(1, Page::Docker)), None);
        assert_eq!(cache.get(&(1, Page::Processes)), Some(12));
        cache.forget(|(id, _)| *id == 1);
        assert_eq!(cache.get(&(1, Page::Processes)), None);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn processes_sort_by_column_with_pid_as_tiebreak() {
        let mut a = proc(10);
        a.name = "nginx".into();
        a.rss = 300;
        a.cpu = Some(5.);
        a.user = Some("www-data".into());
        let mut b = proc(2);
        b.name = "Bash".into();
        b.rss = 100;
        b.cpu = Some(50.);
        let mut c = proc(30);
        c.name = "sshd".into();
        c.rss = 200;
        c.user = Some("root".into());
        c.uid = Some(0);
        let list = [a, b, c];
        let sorted = |key, desc| {
            let mut rows: Vec<usize> = (0..list.len()).collect();
            sort_processes(&list, &mut rows, SortOrder { key, desc });
            rows.iter().map(|&i| list[i].pid).collect::<Vec<_>>()
        };
        assert_eq!(sorted(ProcSort::Name, false), [2, 10, 30]);
        assert_eq!(sorted(ProcSort::Mem, true), [10, 30, 2]);
        // 没有 CPU 读数的排在最后（降序）
        assert_eq!(sorted(ProcSort::Cpu, true), [2, 10, 30]);
        assert_eq!(sorted(ProcSort::Pid, false), [2, 10, 30]);
        assert_eq!(sorted(ProcSort::User, false), [30, 2, 10]);
        // 值相同时按 PID 升序（降序时也是）
        let same = [proc(9), proc(3), proc(5)];
        let mut rows: Vec<usize> = (0..same.len()).collect();
        sort_processes(&same, &mut rows, SortOrder::default());
        assert_eq!(
            rows.iter().map(|&i| same[i].pid).collect::<Vec<_>>(),
            [3, 5, 9]
        );
    }

    #[test]
    fn clicking_a_column_toggles_or_picks_its_default_direction() {
        let order = SortOrder::default();
        assert_eq!(order.key, ProcSort::Cpu);
        assert!(order.desc);
        let order = order.toggled(ProcSort::Cpu);
        assert!(!order.desc, "同一列切换升降");
        assert!(!order.toggled(ProcSort::Name).desc, "名称默认升序");
        assert!(!order.toggled(ProcSort::User).desc, "用户默认升序");
        assert!(order.toggled(ProcSort::Mem).desc, "内存默认降序");
        assert!(order.toggled(ProcSort::Pid).desc, "PID 默认降序");
    }

    #[test]
    fn processes_search_by_name_pid_and_user() {
        let mut a = proc(10);
        a.name = "nginx".into();
        let mut b = proc(30);
        b.name = "sshd".into();
        b.user = Some("root".into());
        let mut c = proc(40);
        c.user = None;
        c.uid = Some(1234);
        let list = [a, b, c];
        let pids = |q: &str| {
            filter_processes(&list, q, &ProcFilter::default())
                .iter()
                .map(|&i| list[i].pid)
                .collect::<Vec<_>>()
        };
        assert_eq!(pids("NGI"), [10]);
        assert_eq!(pids("30"), [30]);
        assert_eq!(pids("root sshd"), [30]);
        assert_eq!(pids("1234"), [40], "没有用户名时按 UID");
        assert_eq!(pids("  ").len(), 3);
    }

    /// 合并同名进程：同名的合成一组（成员沿用排序后的顺序），CPU、内存是合计；按 CPU、内存排序时各组按合计排，
    /// 其余按第一个成员的位置。
    #[test]
    fn same_name_processes_are_grouped() {
        let p = |pid: u32, name: &str, cpu: Option<f64>, rss: u64| Process {
            name: name.into(),
            cpu,
            rss,
            ..proc(pid)
        };
        let list = [
            p(1, "nginx", Some(1.), 100),
            p(2, "sshd", Some(5.), 10),
            p(3, "nginx", Some(4.5), 100),
            p(4, "cron", None, 999),
            p(5, "nginx", None, 100),
        ];
        let order = SortOrder::default();
        let mut rows: Vec<usize> = (0..list.len()).collect();
        sort_processes(&list, &mut rows, order);
        let groups = group_processes(&list, &rows, order);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["nginx", "sshd", "cron"], "按 CPU 的合计");
        let nginx = &groups[0];
        assert_eq!(nginx.cpu, Some(5.5));
        assert_eq!(nginx.rss, 300);
        let pids: Vec<u32> = nginx.members.iter().map(|&i| list[i].pid).collect();
        assert_eq!(pids, [3, 1, 5], "成员沿用排序后的顺序");
        assert_eq!(groups[2].cpu, None, "都没有读数");

        let by_mem = SortOrder {
            key: ProcSort::Mem,
            desc: true,
        };
        let mut rows: Vec<usize> = (0..list.len()).collect();
        sort_processes(&list, &mut rows, by_mem);
        let names: Vec<String> = group_processes(&list, &rows, by_mem)
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["cron", "nginx", "sshd"]);

        let by_name = SortOrder {
            key: ProcSort::Name,
            desc: false,
        };
        let mut rows: Vec<usize> = (0..list.len()).collect();
        sort_processes(&list, &mut rows, by_name);
        let names: Vec<String> = group_processes(&list, &rows, by_name)
            .into_iter()
            .map(|g| g.name)
            .collect();
        assert_eq!(names, ["cron", "nginx", "sshd"]);
    }

    /// 树形的进程：(PID, PPID, 启动时刻, CPU)。
    fn tree_list(items: &[(u32, u32, u64, f64)]) -> Vec<Process> {
        items
            .iter()
            .map(|&(pid, ppid, start, cpu)| Process {
                ppid,
                start_ticks: start,
                cpu: Some(cpu),
                ..proc(pid)
            })
            .collect()
    }

    /// 一行行写成“层级:PID”，看树的形状。
    fn shape(list: &[Process], rows: &[TreeRow]) -> Vec<String> {
        rows.iter()
            .map(|r| format!("{}:{}", r.depth, list[r.ix].pid))
            .collect()
    }

    /// 树形：按父子关系缩进，同一层内按排序（CPU 从高到低）；父进程不在列表中（内核线程的子进程、列表之外）的在
    /// 最上层；收起的只列出自己，写着有几个子进程。
    #[test]
    fn process_tree_nests_and_sorts_siblings() {
        let list = tree_list(&[
            (1, 0, 1, 0.1),
            (42, 1, 5, 1.),
            (43, 42, 6, 0.),
            (44, 42, 7, 3.),
            (900, 1, 8, 2.),
            (950, 900, 9, 0.),
            (3000, 2, 10, 9.), // 父进程是内核线程（不在列表中）
        ]);
        let parents = process_parents(&list);
        let all: Vec<usize> = (0..list.len()).collect();
        let order = SortOrder::default();
        let rows = process_tree(&list, &parents, &all, order, &HashSet::new());
        assert_eq!(
            shape(&list, &rows),
            ["0:3000", "0:1", "1:900", "2:950", "1:42", "2:44", "2:43"],
            "同一层内按 CPU 从高到低"
        );
        let nginx = rows.iter().find(|r| list[r.ix].pid == 42).unwrap();
        assert_eq!(
            (nginx.children, nginx.collapsed, nginx.context),
            (2, false, false)
        );
        assert_eq!(nginx.parent.map(|p| list[p].pid), Some(1));

        let by_pid = SortOrder {
            key: ProcSort::Pid,
            desc: false,
        };
        let collapsed: HashSet<ProcKey> = [proc_key(&list[1])].into_iter().collect();
        let rows = process_tree(&list, &parents, &all, by_pid, &collapsed);
        assert_eq!(
            shape(&list, &rows),
            ["0:1", "1:42", "1:900", "2:950", "0:3000"],
            "收起的 42 不列出子进程"
        );
        let nginx = rows.iter().find(|r| list[r.ix].pid == 42).unwrap();
        assert!(nginx.collapsed && nginx.children == 2);
    }

    /// PID 被重新使用（父进程比子进程晚启动）的不算父进程；自己是自己的父进程、读取不一致连成的环都在最上层断开，
    /// 不会无限循环。
    #[test]
    fn process_tree_survives_pid_reuse_and_cycles() {
        let list = tree_list(&[
            (1, 0, 1, 0.),
            (10, 1, 50, 0.),  // 新进程重新使用了 PID 10
            (11, 10, 20, 0.), // 原来的父进程 10 已结束：比它晚启动的 10 不是它的父进程
            (20, 21, 30, 0.), // 20 与 21 互为父进程（读取不一致）
            (21, 20, 30, 0.),
            (30, 30, 40, 0.), // 自己是自己的父进程
        ]);
        let parents = process_parents(&list);
        assert_eq!(parents[2], None, "PID 被重新使用");
        assert_eq!(parents[1], Some(0));
        assert_eq!(parents[3], None, "环在 PID 最小处断开");
        assert_eq!(parents[4], Some(3));
        assert_eq!(parents[5], None);
        let all: Vec<usize> = (0..list.len()).collect();
        let by_pid = SortOrder {
            key: ProcSort::Pid,
            desc: false,
        };
        let rows = process_tree(&list, &parents, &all, by_pid, &HashSet::new());
        assert_eq!(
            shape(&list, &rows),
            ["0:1", "1:10", "0:11", "0:20", "1:21", "0:30"],
            "每个进程正好一行"
        );
    }

    /// 搜索、筛选时列出命中的进程与它们的各级父进程：父进程只是为了看出位置（context），不算命中；`ancestors`
    /// 给出要展开的父进程。
    #[test]
    fn process_tree_keeps_matches_with_their_parents() {
        let list = tree_list(&[
            (1, 0, 1, 0.),
            (900, 1, 2, 0.),
            (950, 900, 3, 0.),
            (951, 950, 4, 0.),
            (42, 1, 5, 0.),
        ]);
        let parents = process_parents(&list);
        let by_pid = SortOrder {
            key: ProcSort::Pid,
            desc: false,
        };
        let matched = [3usize]; // 951
        let rows = process_tree(&list, &parents, &matched, by_pid, &HashSet::new());
        assert_eq!(shape(&list, &rows), ["0:1", "1:900", "2:950", "3:951"]);
        let context: Vec<bool> = rows.iter().map(|r| r.context).collect();
        assert_eq!(context, [true, true, true, false]);
        let up: Vec<u32> = {
            let mut pids: Vec<u32> = ancestors(&parents, &matched)
                .into_iter()
                .map(|ix| list[ix].pid)
                .collect();
            pids.sort();
            pids
        };
        assert_eq!(up, [1, 900, 950]);
    }

    /// 按用户筛选：下拉菜单按进程数列出用户（没有用户名的写 UID）；筛选条件与搜索、隐藏容器一起生效。
    #[test]
    fn processes_filter_by_user() {
        let user = |pid: u32, name: Option<&str>, uid: u32| Process {
            user: name.map(Into::into),
            uid: Some(uid),
            ..proc(pid)
        };
        let list = [
            user(1, Some("root"), 0),
            user(2, Some("root"), 0),
            user(3, Some("www-data"), 33),
            user(4, None, 1234),
            user(5, Some("root"), 0),
            user(6, Some("alice"), 1000),
        ];
        assert_eq!(
            process_users(&list),
            [
                ("root".to_string(), 3),
                ("1234".to_string(), 1),
                ("alice".to_string(), 1),
                ("www-data".to_string(), 1)
            ]
        );
        let pids = |q: &str, filter: &ProcFilter| -> Vec<u32> {
            filter_processes(&list, q, filter)
                .iter()
                .map(|&i| list[i].pid)
                .collect()
        };
        let root = ProcFilter {
            user: Some("root".into()),
            ..ProcFilter::default()
        };
        assert_eq!(pids("", &root), [1, 2, 5]);
        assert_eq!(pids("p2", &root), [2], "与搜索一起");
        let uid = ProcFilter {
            user: Some("1234".into()),
            ..ProcFilter::default()
        };
        assert_eq!(pids("", &uid), [4], "没有用户名时按 UID");
        assert!(root.matches(&list[0]) && !root.matches(&list[2]));
    }

    /// 定时器：在计时的不再启动，没在计时的不能停止；开机启用、禁用看单元文件；重启、重新加载不适用。搜索名称、描述与
    /// 触发的服务。服务详情找出触发它的定时器。
    #[test]
    fn timers_ops_search_and_owners() {
        let timer = |unit: &str, active: &str, boot: &str, activates: &str| Timer {
            unit: unit.into(),
            description: format!("{unit} desc"),
            active_state: active.into(),
            unit_file_state: boot.into(),
            activates: activates.into(),
            ..Default::default()
        };
        let backup = timer("backup.timer", "active", "enabled", "backup.service");
        let report = timer(
            "report.timer",
            "inactive",
            "disabled",
            "send-report.service",
        );
        let ops = |t: &Timer| -> Vec<ServiceOp> {
            TIMER_OPS
                .into_iter()
                .filter(|op| timer_op_applies(t, *op))
                .collect()
        };
        assert_eq!(ops(&backup), [ServiceOp::Stop, ServiceOp::Disable]);
        assert_eq!(ops(&report), [ServiceOp::Start, ServiceOp::Enable]);
        assert!(!timer_op_applies(&backup, ServiceOp::Restart));
        let list = [backup, report];
        assert_eq!(filter_timers(&list, "send-report"), [1], "按触发的服务");
        assert_eq!(filter_timers(&list, "BACKUP desc"), [0]);
        assert_eq!(filter_timers(&list, ""), [0, 1]);
        assert_eq!(timers_of(&list, "backup.service").len(), 1);
        assert!(timers_of(&list, "report.service").is_empty());
        assert_eq!(timer_name("backup.timer"), "backup");
        assert!(filter_services(&[], ServiceFilter::Timers, true, "").is_empty());
    }

    const ID: &str = "c07d8b88b8c6b55cb13f9e92654df607a79449021ceeeb816e058a36b291dc83";

    /// 隐藏容器内的进程、只看一个容器内的进程（这时不看隐藏）；搜索也按所属服务。
    #[test]
    fn processes_filter_by_container_and_search_services() {
        let web = Process {
            container: Some(ID.into()),
            name: "nginx".into(),
            ..proc(10)
        };
        let db = Process {
            container: Some("f".repeat(64)),
            ..proc(11)
        };
        let host = Process {
            service: Some("cron.service".into()),
            ..proc(12)
        };
        let list = [web, db, host];
        let pids = |q: &str, filter: ProcFilter| {
            filter_processes(&list, q, &filter)
                .iter()
                .map(|&i| list[i].pid)
                .collect::<Vec<_>>()
        };
        assert_eq!(pids("", ProcFilter::default()), [10, 11, 12]);
        let hide = ProcFilter {
            hide_containers: true,
            ..ProcFilter::default()
        };
        assert_eq!(pids("", hide.clone()), [12]);
        let one = ProcFilter {
            hide_containers: true,
            container: Some(ID.into()),
            ..ProcFilter::default()
        };
        assert_eq!(pids("", one.clone()), [10], "只看这个容器时不隐藏它");
        assert_eq!(pids("ngi", one), [10]);
        assert_eq!(pids("cron", ProcFilter::default()), [12], "按所属服务搜索");
        assert!(pids("cron.service", ProcFilter::default()).is_empty());
    }

    /// 容器名：Docker 页读到的名称优先，其次是进程页自己读到的，都没有时用短 ID。
    #[test]
    fn container_labels_use_docker_names() {
        let info = DockerInfo {
            engine: termlet_core::hosttools::Engine::Docker,
            sudo: false,
            version: None,
            compose: None,
            containers: vec![Container {
                id: ID.into(),
                ..container("shop-web-1", Some("shop"), "running")
            }],
            images: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
        };
        let names: HashMap<String, String> = [
            (ID.to_string(), "old-name".to_string()),
            ("b".repeat(64), "cache".to_string()),
        ]
        .into();
        assert_eq!(container_label(ID, Some(&info), Some(&names)), "shop-web-1");
        assert_eq!(container_label(ID, None, None), "c07d8b88b8c6");
        assert_eq!(container_label(ID, None, Some(&names)), "old-name");
        assert_eq!(
            container_label(&"b".repeat(64), Some(&info), Some(&names)),
            "cache",
            "Docker 页的读数中没有时用自己读到的"
        );
        assert_eq!(
            container_label(&"a".repeat(64), Some(&info), Some(&names)),
            "aaaaaaaaaaaa"
        );
    }

    /// 键盘移动当前行：到头停住，项目标题跳过，没有当前行时从第一个能选的开始。
    #[test]
    fn keyboard_steps_skip_unselectable_rows() {
        let all = |_: usize| true;
        assert_eq!(step_row(None, 0, Step::Next, 5, all), None);
        assert_eq!(step_row(None, 4, Step::Prev, 5, all), Some(0));
        assert_eq!(step_row(Some(0), 4, Step::Prev, 5, all), Some(0));
        assert_eq!(step_row(Some(3), 4, Step::Next, 5, all), Some(3));
        assert_eq!(step_row(Some(1), 10, Step::PageDown, 3, all), Some(4));
        assert_eq!(step_row(Some(9), 10, Step::PageDown, 3, all), Some(9));
        assert_eq!(step_row(Some(5), 10, Step::PageUp, 3, all), Some(2));
        assert_eq!(step_row(Some(5), 10, Step::Last, 3, all), Some(9));
        assert_eq!(step_row(Some(5), 10, Step::First, 3, all), Some(0));
        assert_eq!(
            step_row(Some(99), 10, Step::Prev, 3, all),
            Some(8),
            "当前行超出时按最后一行算"
        );
        // 0、3 是项目的标题
        let rows = |ix: usize| ix != 0 && ix != 3;
        assert_eq!(step_row(None, 5, Step::Next, 3, rows), Some(1));
        assert_eq!(step_row(Some(2), 5, Step::Next, 3, rows), Some(4));
        assert_eq!(step_row(Some(4), 5, Step::Prev, 3, rows), Some(2));
        assert_eq!(
            step_row(Some(1), 5, Step::Prev, 3, rows),
            Some(1),
            "上面只有标题时不动"
        );
        assert_eq!(step_row(Some(4), 5, Step::First, 3, rows), Some(1));
        assert_eq!(step_row(Some(1), 5, Step::Last, 3, |ix| ix == 1), Some(1));
        assert_eq!(step_row(None, 3, Step::Next, 3, |_| false), None);
        assert_eq!(page_rows(150., 24.), 5);
        assert_eq!(page_rows(10., 24.), 1);
        assert_eq!(page_rows(100., 0.), 1);
    }

    /// 容器发布的端口：两种 IP 写一次、端口范围展开、没有发布的不算；运行中的容器优先。
    #[test]
    fn published_ports_map_to_containers() {
        assert_eq!(
            published_host_ports(
                "0.0.0.0:8081->80/tcp, :::8081->80/tcp, 6379/tcp, 127.0.0.1:5353->53/udp"
            ),
            [("tcp".to_string(), 8081), ("udp".to_string(), 5353)]
        );
        assert_eq!(
            published_host_ports("0.0.0.0:8000-8002->8000-8002/tcp"),
            [
                ("tcp".to_string(), 8000),
                ("tcp".to_string(), 8001),
                ("tcp".to_string(), 8002)
            ]
        );
        assert_eq!(published_host_ports("[::]:443->443/tcp").len(), 1);
        assert_eq!(
            published_host_ports("0.0.0.0:1-65535->1-65535/tcp").len(),
            MAX_RANGE as usize,
            "大范围只认前面的"
        );
        assert!(published_host_ports("").is_empty());
        let web = Container {
            ports: "0.0.0.0:8081->80/tcp".into(),
            ..container("old-web", None, "exited")
        };
        let new = Container {
            ports: "0.0.0.0:8081->80/tcp, 0.0.0.0:9000->9000/tcp".into(),
            ..container("web", None, "running")
        };
        let map = port_owners(&[web, new]);
        assert_eq!(
            map.get(&("tcp".to_string(), 8081)),
            Some(&1),
            "运行中的优先"
        );
        assert_eq!(map.get(&("tcp".to_string(), 9000)), Some(&1));
        assert_eq!(map.get(&("udp".to_string(), 8081)), None);
        assert!(is_port_proxy("docker-proxy") && !is_port_proxy("nginx"));
    }

    /// 监听视图：同一端口的 IPv4、IPv6 合成一行，进程去重，按端口排序；范围写成“全部地址”等。
    #[test]
    fn listening_ports_merge_ipv4_and_ipv6() {
        let mut list = conns();
        let mut sshd6 = conn("tcp", "LISTEN", "::", 22);
        sshd6.processes = list[0].processes.clone();
        list.push(sshd6);
        let mut local = conn("tcp", "LISTEN", "127.0.0.1", 631);
        local.processes = vec![ConnProcess {
            pid: 5,
            name: "cupsd".into(),
        }];
        let mut local6 = conn("tcp", "LISTEN", "::1", 631);
        local6.processes = vec![ConnProcess {
            pid: 6,
            name: "cupsd".into(),
        }];
        list.extend([local, local6]);
        let rows = filter_conns(&list, ConnFilter::Listen, None, "");
        let merged = listen_rows(&list, &rows);
        let summary: Vec<(&str, u16, Vec<&str>, usize)> = merged
            .iter()
            .map(|r| {
                (
                    r.proto.as_str(),
                    r.port,
                    r.addrs.iter().map(String::as_str).collect(),
                    r.processes.len(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("tcp", 22, vec!["0.0.0.0", "::"], 1),
                ("udp", 53, vec!["127.0.0.53"], 0),
                ("tcp", 80, vec!["::"], 1),
                ("tcp", 631, vec!["127.0.0.1", "::1"], 2),
            ]
        );
        let keys: HashSet<&str> = merged.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys.len(), merged.len(), "元素 ID 不重复");
        assert_eq!(
            merged[0]
                .forward_conn(&list, &rows)
                .map(|c| c.local_addr.as_str()),
            Some("0.0.0.0")
        );
        assert!(
            merged[1].forward_conn(&list, &rows).is_none(),
            "UDP 不能转发"
        );
        let strings = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(bind_parts(&strings(&["0.0.0.0", "::"])), [BindPart::Any]);
        assert_eq!(bind_parts(&strings(&["*"])), [BindPart::Any]);
        assert_eq!(bind_parts(&strings(&["0.0.0.0"])), [BindPart::AnyV4]);
        assert_eq!(bind_parts(&strings(&["::"])), [BindPart::AnyV6]);
        assert_eq!(
            bind_parts(&strings(&["127.0.0.1", "::1"])),
            [
                BindPart::Addr("127.0.0.1".into()),
                BindPart::Addr("::1".into())
            ]
        );
        assert_eq!(
            bind_parts(&strings(&["::", "10.0.0.5"])),
            [BindPart::AnyV6, BindPart::Addr("10.0.0.5".into())]
        );
    }

    /// 网络页的筛选条件：按进程、按本机端口。
    #[test]
    fn connection_chips_filter_by_pid_and_port() {
        let list = conns();
        let by_pid = ConnChip::Pid {
            pid: 1001,
            name: Some("sshd".into()),
        };
        assert_eq!(filter_conns(&list, ConnFilter::All, Some(&by_pid), ""), [0]);
        assert_eq!(
            filter_conns(&list, ConnFilter::All, Some(&ConnChip::Port(22)), ""),
            [0, 1],
            "监听与已连接的都算"
        );
        assert!(
            filter_conns(&list, ConnFilter::Udp, Some(&ConnChip::Port(22)), "").is_empty(),
            "与页签的筛选一起生效"
        );
    }

    #[test]
    fn descendants_include_grandchildren() {
        let tree: Vec<Process> = [(1, 0), (5, 1), (6, 5), (7, 6), (8, 1)]
            .into_iter()
            .map(|(pid, ppid)| Process { ppid, ..proc(pid) })
            .collect();
        let mut kids: Vec<u32> = descendants(&tree, 5).into_iter().collect();
        kids.sort();
        assert_eq!(kids, [6, 7]);
        assert!(descendants(&tree, 7).is_empty());
    }

    #[test]
    fn process_risk_in_order() {
        let named = |pid, name: &str| Process {
            name: name.into(),
            ..proc(pid)
        };
        assert_eq!(process_risk(&named(1, "systemd")), ProcessRisk::Init);
        let shell = Process {
            in_terminal: true,
            terminal_shell: true,
            ..named(400, "bash")
        };
        assert_eq!(process_risk(&shell), ProcessRisk::Shell);
        let vim = Process {
            in_terminal: true,
            ..named(401, "vim")
        };
        assert_eq!(process_risk(&vim), ProcessRisk::InTerminal);
        assert_eq!(process_risk(&named(900, "sshd")), ProcessRisk::Sshd);
        assert_eq!(process_risk(&named(901, "sshd-session")), ProcessRisk::Sshd);
        assert_eq!(process_risk(&named(903, "dropbear")), ProcessRisk::Sshd);
        assert_eq!(process_risk(&named(902, "ssh")), ProcessRisk::Normal);
    }

    #[test]
    fn cpu_and_clock_formats() {
        assert_eq!(format_cpu(None), "-");
        assert_eq!(format_cpu(Some(12.345)), "12.3%");
        assert_eq!(format_cpu(Some(0.)), "0.0%");
        let text = clock_time(1_700_000_007_000);
        assert_eq!(text.len(), 8, "{text}");
        assert!(text.ends_with(":27"), "{text}");
    }

    /// 服务的失败原因用 systemd 的原词：退出码、信号（常见的写名称）、其他结果原样；读不到时没有。
    #[test]
    fn service_results_name_the_exit() {
        let failed = |result: &str, main_exit| Service {
            result: result.into(),
            main_exit,
            ..Default::default()
        };
        assert_eq!(
            service_result(&failed("exit-code", Some(MainExit::Code(3)))).as_deref(),
            Some("exit-code 3")
        );
        assert_eq!(
            service_result(&failed("signal", Some(MainExit::Signal(9)))).as_deref(),
            Some("signal SIGKILL")
        );
        assert_eq!(
            service_result(&failed("core-dump", Some(MainExit::Signal(64)))).as_deref(),
            Some("core-dump 64")
        );
        assert_eq!(
            service_result(&failed("start-limit-hit", Some(MainExit::Code(3)))).as_deref(),
            Some("start-limit-hit (exit-code 3)"),
            "反复失败后达到启动上限：括注最后一次的退出码"
        );
        assert_eq!(
            service_result(&failed("timeout", Some(MainExit::Signal(15)))).as_deref(),
            Some("timeout (signal SIGTERM)")
        );
        assert_eq!(
            service_result(&failed("timeout", Some(MainExit::Code(0)))).as_deref(),
            Some("timeout")
        );
        assert_eq!(service_result(&failed("", None)), None);
    }

    /// 事件的时间：今天的只写时分，别的日子带上日期（UTC 06:00 前后一分钟在任何时区都是同一天）。
    #[test]
    fn event_times_add_the_date_on_other_days() {
        let at = 1_793_772_000_000; // 2026-11-04 06:00:00 UTC
        let today = event_time(at, at + 60_000, DateStyle::Dash);
        assert_eq!(today.len(), 5, "{today}");
        assert!(today.contains(':'), "{today}");
        let earlier = event_time(at, at + 2 * 86_400_000, DateStyle::Dash);
        assert!(earlier.contains('-') && earlier.contains(':'), "{earlier}");
    }

    fn svc(unit: &str, active: &str, boot: &str) -> Service {
        Service {
            unit: unit.into(),
            description: format!("{unit} desc"),
            load_state: "loaded".into(),
            active_state: active.into(),
            sub_state: String::new(),
            unit_file_state: boot.into(),
            fragment_path: None,
            main_pid: None,
            custom: false,
            ..Default::default()
        }
    }

    #[test]
    fn services_filter_by_state_and_search_name_and_description() {
        let list = [
            svc("nginx.service", "active", "enabled"),
            svc("backup.service", "failed", "enabled"),
            svc("cron.service", "inactive", "enabled"),
            svc("db.service", "activating", "enabled"),
        ];
        let units = |filter, q: &str| {
            filter_services(&list, filter, false, q)
                .iter()
                .map(|&i| list[i].unit.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(units(ServiceFilter::All, "").len(), 4);
        assert_eq!(
            units(ServiceFilter::Running, ""),
            ["nginx.service", "db.service"]
        );
        assert_eq!(units(ServiceFilter::Stopped, ""), ["cron.service"]);
        assert_eq!(units(ServiceFilter::Failed, ""), ["backup.service"]);
        assert_eq!(units(ServiceFilter::All, "CRON desc"), ["cron.service"]);
    }

    /// 默认不列出 not-found 的单元与已停止的静态单元（失败的、运行中的照列）；“显示全部”时都列出。
    #[test]
    fn not_found_and_stopped_static_units_are_hidden_by_default() {
        let not_found = Service {
            load_state: "not-found".into(),
            ..svc("gone.service", "inactive", "")
        };
        let list = [
            svc("nginx.service", "active", "enabled"),
            not_found,
            svc("apt-daily.service", "inactive", "static"),
            svc("dbus.service", "active", "static"),
            svc("broken.service", "failed", "static"),
        ];
        let units = |show_all| {
            filter_services(&list, ServiceFilter::All, show_all, "")
                .iter()
                .map(|&i| list[i].unit.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            units(false),
            ["nginx.service", "dbus.service", "broken.service"]
        );
        assert_eq!(units(true).len(), 5);
    }

    /// 有自定义的服务时分成两组（各有标题与数量），都是系统服务时不分组；版本只取前两个词。
    #[test]
    fn services_are_grouped_into_custom_and_system() {
        let custom = |unit: &str| Service {
            custom: true,
            ..svc(unit, "active", "enabled")
        };
        let list = [
            custom("app.service"),
            custom("worker.service"),
            svc("cron.service", "active", "enabled"),
        ];
        assert_eq!(
            service_rows(&list, &[0, 1, 2]),
            [
                ServiceRow::Header {
                    custom: true,
                    count: 2
                },
                ServiceRow::Service(0),
                ServiceRow::Service(1),
                ServiceRow::Header {
                    custom: false,
                    count: 1
                },
                ServiceRow::Service(2),
            ]
        );
        assert_eq!(service_rows(&list, &[2]), [ServiceRow::Service(2)]);
        assert_eq!(
            service_rows(&list, &[1]),
            [
                ServiceRow::Header {
                    custom: true,
                    count: 1
                },
                ServiceRow::Service(1)
            ]
        );
        assert_eq!(
            systemd_version("systemd 257 (257.9-1~deb13u1)"),
            "systemd 257"
        );
        assert_eq!(systemd_version("systemd 219"), "systemd 219");
    }

    #[test]
    fn service_ops_follow_the_state() {
        let running = svc("a.service", "active", "enabled");
        let stopped = svc("b.service", "inactive", "disabled");
        let static_unit = svc("c.service", "inactive", "static");
        let ops = |s: &Service| {
            SERVICE_OPS
                .into_iter()
                .filter(|op| service_op_applies(s, *op))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ops(&running),
            [
                ServiceOp::Stop,
                ServiceOp::Restart,
                ServiceOp::Reload,
                ServiceOp::Disable
            ]
        );
        assert_eq!(
            ops(&stopped),
            [ServiceOp::Start, ServiceOp::Restart, ServiceOp::Enable]
        );
        assert_eq!(ops(&static_unit), [ServiceOp::Start, ServiceOp::Restart]);
    }

    #[test]
    fn service_risk_levels() {
        for op in SERVICE_OPS {
            assert_eq!(service_risk("nginx.service", op), ServiceRisk::Confirm);
        }
        // 启动、重新加载防火墙也可能把自己挡在外面
        assert_eq!(
            service_risk("firewalld.service", ServiceOp::Start),
            ServiceRisk::Connection
        );
        assert_eq!(
            service_risk("firewalld.service", ServiceOp::Reload),
            ServiceRisk::Connection
        );
        assert_eq!(
            service_risk("ufw.service", ServiceOp::Disable),
            ServiceRisk::Connection
        );
        assert_eq!(
            service_risk("firewalld.service", ServiceOp::Enable),
            ServiceRisk::Confirm
        );
        for unit in [
            "systemd-poweroff.service",
            "systemd-reboot.service",
            "systemd-halt.service",
            "systemd-kexec.service",
            "systemd-suspend.service",
            "systemd-hibernate.service",
            "systemd-hybrid-sleep.service",
            "systemd-suspend-then-hibernate.service",
        ] {
            assert_eq!(service_risk(unit, ServiceOp::Start), ServiceRisk::Power);
            assert_eq!(service_risk(unit, ServiceOp::Restart), ServiceRisk::Power);
        }
        assert_eq!(
            service_risk("systemd-reboot-helper.service", ServiceOp::Start),
            ServiceRisk::Confirm
        );
        for unit in [
            "ssh.service",
            "sshd.service",
            "NetworkManager.service",
            "networking.service",
            "systemd-networkd.service",
            "firewalld.service",
            "wg-quick@wg0.service",
        ] {
            assert_eq!(service_risk(unit, ServiceOp::Stop), ServiceRisk::Connection);
            assert_eq!(
                service_risk(unit, ServiceOp::Restart),
                ServiceRisk::Connection
            );
        }
        assert_eq!(
            service_risk("ssh.service", ServiceOp::Start),
            ServiceRisk::Connection
        );
        assert_eq!(
            service_risk("sshguard.service", ServiceOp::Stop),
            ServiceRisk::Confirm
        );
        assert_eq!(service_name("nginx.service"), "nginx");
        assert_eq!(service_name("odd"), "odd");
    }

    fn container(name: &str, project: Option<&str>, state: &str) -> Container {
        Container {
            id: format!("{name}-id"),
            name: name.into(),
            image: "nginx".into(),
            state: state.into(),
            status: String::new(),
            ports: String::new(),
            created: String::new(),
            project: project.map(Into::into),
            service: None,
            working_dir: project.map(|p| format!("/srv/{p}")),
            run: None,
        }
    }

    #[test]
    fn containers_group_by_project_with_standalone_last() {
        let list = [
            container("solo", None, "exited"),
            container("shop-web-1", Some("shop"), "running"),
            container("blog-web-1", Some("blog"), "exited"),
            container("shop-db-1", Some("shop"), "paused"),
        ];
        let groups = container_groups(&list, "", false, None);
        assert_eq!(
            groups.iter().map(|g| g.project).collect::<Vec<_>>(),
            [Some("blog"), Some("shop"), None]
        );
        assert_eq!(groups[1].running, 2, "paused 也算运行中");
        assert_eq!(groups[1].working_dir, Some("/srv/shop"));
        assert_eq!(groups[1].containers.len(), 2);
        assert_eq!(groups[0].running, 0);
        let only = [
            container("a", Some("shop"), "running"),
            container("b", Some("blog"), "running"),
        ];
        assert_eq!(
            container_groups(&only, "blog", false, None)
                .iter()
                .map(|g| g.project)
                .collect::<Vec<_>>(),
            [Some("blog")]
        );
    }

    /// 异常：非 0 退出（警告）、不健康、重启中、已失效（危险）；正常退出、运行中的不算。
    #[test]
    fn container_problems() {
        let with = |state: &str, status: &str| Container {
            status: status.into(),
            ..container("x", None, state)
        };
        assert_eq!(
            container_problem(&with("exited", "Exited (1) 10 minutes ago")),
            Some(ContainerProblem::Exited(1))
        );
        assert_eq!(
            container_problem(&with("exited", "Exited (0) 10 minutes ago")),
            None
        );
        assert_eq!(
            container_problem(&with("running", "Up 3 minutes (unhealthy)")),
            Some(ContainerProblem::Unhealthy)
        );
        assert_eq!(
            container_problem(&with("running", "Up 3 minutes (healthy)")),
            None
        );
        assert_eq!(
            container_problem(&with("restarting", "Restarting (1) 2 seconds ago")),
            Some(ContainerProblem::Restarting)
        );
        assert_eq!(
            container_problem(&with("dead", "Dead")),
            Some(ContainerProblem::Dead)
        );
        assert!(!ContainerProblem::Exited(1).is_danger());
        assert!(ContainerProblem::Unhealthy.is_danger());
        // 只看异常的：分组照旧
        let list = [
            with("exited", "Exited (1) 1 hour ago"),
            Container {
                project: Some("shop".into()),
                ..with("running", "Up 1 hour (unhealthy)")
            },
            with("running", "Up 1 hour"),
        ];
        let groups = container_groups(&list, "", true, None);
        let count: usize = groups.iter().map(|g| g.containers.len()).sum();
        assert_eq!(count, 2);
        assert_eq!(groups[0].project, Some("shop"));
    }

    /// 按 CPU 排序：组内从高到低，各组按组内最高的；没有读数的在后。
    #[test]
    fn containers_sort_by_usage() {
        let list = [
            container("a", Some("blog"), "running"),
            container("b", Some("shop"), "running"),
            container("c", Some("shop"), "running"),
            container("d", None, "exited"),
        ];
        let cpu = |c: &Container| match c.name.as_str() {
            "a" => Some(5.),
            "b" => Some(1.),
            "c" => Some(80.),
            _ => None,
        };
        let groups = container_groups(&list, "", false, Some(&cpu));
        let names: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.containers.iter().map(|c| c.name.as_str()).collect())
            .collect();
        assert_eq!(names, [vec!["c", "b"], vec!["a"], vec!["d"]]);
    }

    /// 列表中的端口：精简写法；详情中的端口与“转发到本机”的预填。
    #[test]
    fn ports_compact_and_forward() {
        assert_eq!(
            compact_ports(
                "0.0.0.0:8081->80/tcp, :::8081->80/tcp, 6379/tcp, 127.0.0.1:5353->53/udp"
            ),
            "8081→80, 6379, 127.0.0.1:5353→53/udp"
        );
        assert_eq!(compact_ports("[::]:443->443/tcp"), "443→443");
        assert_eq!(compact_ports(""), "");
        let published = PortBinding {
            container_port: 80,
            proto: "tcp".into(),
            host_port: Some(8081),
            host_ip: None,
        };
        assert_eq!(published_port(&published).unwrap(), "8081 → 80/tcp");
        let preset = container_forward_preset(&published, "shop-web-1").unwrap();
        assert_eq!(
            (
                preset.target_host.as_str(),
                preset.port,
                preset.name.as_str()
            ),
            ("127.0.0.1", 8081, "shop-web-1")
        );
        let local = PortBinding {
            host_ip: Some("192.168.1.5".into()),
            ..published.clone()
        };
        assert_eq!(published_port(&local).unwrap(), "192.168.1.5:8081 → 80/tcp");
        assert_eq!(
            container_forward_preset(&local, "x").unwrap().target_host,
            "192.168.1.5"
        );
        let udp = PortBinding {
            proto: "udp".into(),
            ..published.clone()
        };
        assert!(
            container_forward_preset(&udp, "x").is_none(),
            "UDP 不能经 SSH 转发"
        );
        let internal = PortBinding {
            host_port: None,
            ..published
        };
        assert!(published_port(&internal).is_none());
        assert!(container_forward_preset(&internal, "x").is_none());
    }

    #[test]
    fn removable_items_and_image_names() {
        let image = Image {
            id: "sha256:0123456789abcdef".into(),
            repository: "<none>".into(),
            tag: "<none>".into(),
            size: "1MB".into(),
            created: String::new(),
            created_at: None,
            containers: 0,
            in_use: false,
        };
        assert!(image_removable(&image));
        assert!(!image_removable(&Image {
            in_use: true,
            ..image.clone()
        }));
        assert_eq!(image_name(&image), "0123456789ab");
        assert_eq!(
            image_name(&Image {
                repository: "nginx".into(),
                tag: "1.27".into(),
                ..image.clone()
            }),
            "nginx:1.27"
        );
        let net = Network {
            id: "x".into(),
            name: "bridge".into(),
            driver: "bridge".into(),
            scope: "local".into(),
            builtin: true,
            containers: 0,
            in_use: false,
        };
        assert!(!network_removable(&net), "内置的网络不能删除");
        assert!(network_removable(&Network {
            builtin: false,
            ..net
        }));
        assert_eq!(short_id("abc"), "abc");
        assert!(matches_any("NGIN", &["nginx", "latest"]));
        assert!(matches_any("", &[]));
        assert!(
            !matches_any("web app", &["web", "app"]),
            "整个查询当作一个子串"
        );
    }

    fn conn(proto: &str, state: &str, local: &str, port: u16) -> Conn {
        Conn {
            proto: proto.into(),
            state: state.into(),
            local_addr: local.into(),
            local_port: port,
            peer_addr: "0.0.0.0".into(),
            peer_port: 0,
            recv_q: 0,
            send_q: 0,
            processes: Vec::new(),
            uid: Some(0),
            user: None,
        }
    }

    fn conns() -> Vec<Conn> {
        let mut sshd = conn("tcp", "LISTEN", "0.0.0.0", 22);
        sshd.processes = vec![ConnProcess {
            pid: 1001,
            name: "sshd".into(),
        }];
        let mut estab = conn("tcp", "ESTAB", "10.0.0.5", 22);
        estab.peer_addr = "10.0.0.1".into();
        estab.peer_port = 50000;
        let dns = conn("udp", "UNCONN", "127.0.0.53", 53);
        let mut web = conn("tcp", "LISTEN", "::", 80);
        web.processes = vec![ConnProcess {
            pid: 7,
            name: "nginx".into(),
        }];
        vec![sshd, estab, dns, web]
    }

    #[test]
    fn connections_filter_and_search() {
        let list = conns();
        assert_eq!(filter_conns(&list, ConnFilter::Listen, None, "").len(), 3);
        assert_eq!(
            filter_conns(&list, ConnFilter::Established, None, "").len(),
            1
        );
        assert_eq!(filter_conns(&list, ConnFilter::Udp, None, ""), [2]);
        assert_eq!(filter_conns(&list, ConnFilter::Tcp, None, "").len(), 3);
        assert_eq!(filter_conns(&list, ConnFilter::All, None, "nginx"), [3]);
        assert_eq!(filter_conns(&list, ConnFilter::All, None, "[::]:80"), [3]);
        assert_eq!(
            filter_conns(&list, ConnFilter::All, None, "10.0.0.1:50000"),
            [1]
        );
    }

    #[test]
    fn connection_summary_uses_the_totals() {
        let list = ConnList {
            conns: conns(),
            counts: vec![
                ConnCount {
                    proto: "tcp".into(),
                    state: "LISTEN".into(),
                    count: 5,
                },
                ConnCount {
                    proto: "tcp".into(),
                    state: "ESTAB".into(),
                    count: 100,
                },
                ConnCount {
                    proto: "tcp".into(),
                    state: "TIME-WAIT".into(),
                    count: 7,
                },
                ConnCount {
                    proto: "udp".into(),
                    state: "UNCONN".into(),
                    count: 3,
                },
            ],
            total: 115,
            truncated: true,
            source: ConnSource::Ss,
            uid: Some(0),
            sudo: false,
        };
        assert_eq!(
            conn_summary(&list),
            ConnSummary {
                total: 115,
                listen: 8,
                established: 100,
                time_wait: 7,
                tcp: 112,
                udp: 3
            }
        );
    }

    #[test]
    fn offsets_zones_and_web_ports() {
        assert_eq!(utc_offset_label(8 * 3600), "UTC+8");
        assert_eq!(utc_offset_label(-5 * 3600), "UTC-5");
        assert_eq!(utc_offset_label(5 * 3600 + 45 * 60), "UTC+5:45");
        assert_eq!(utc_offset_label(-(3 * 3600 + 30 * 60)), "UTC-3:30");
        assert_eq!(utc_offset_label(0), "UTC");
        let zone = |name: &str, abbr: &str| ServerZone {
            name: name.into(),
            abbr: abbr.into(),
            offset: 0,
        };
        assert_eq!(zone_name(&zone("Europe/Berlin", "CET")), "Europe/Berlin");
        assert_eq!(zone_name(&zone("", "CET")), "CET");
        assert_eq!(zone_name(&zone("", "+08")), "", "数字的缩写不当名称");
        assert!(!calendar_has_zone("*-*-* 03:00:00"));
        assert!(calendar_has_zone("*-*-* 03:00:00 Europe/Berlin"));
        assert!(calendar_has_zone("Mon *-*-* 08:00:00 UTC"));
        assert!(!calendar_has_zone("daily"));
        assert!(is_web_port(8080) && is_web_port(443) && !is_web_port(22));
        let preset = |port: u16| ForwardPreset {
            target_host: "127.0.0.1".into(),
            port,
            name: String::new(),
        };
        assert!(BrowserPreset::new(preset(8443)).https);
        assert!(!BrowserPreset::new(preset(8080)).https);
        assert!(!BrowserPreset::new(preset(7000)).https, "不认识的按 http");
        // 服务器的今天只写时分，别的日子带上日期
        let now = 1_794_200_000_000;
        assert_eq!(
            event_time_at(now + 60_000, now, DateStyle::Dash, Some(0)).len(),
            5
        );
        assert!(event_time_at(now + 3 * 86_400_000, now, DateStyle::Dash, Some(0)).len() > 5);
    }

    #[test]
    fn addresses_and_forward_preset() {
        let list = conns();
        assert_eq!(host_port("::1", 631), "[::1]:631");
        assert_eq!(host_port("0.0.0.0", 0), "0.0.0.0:*");
        assert!(can_forward(&list[0]));
        assert!(!can_forward(&list[1]), "已连接的不能转发");
        assert!(!can_forward(&list[2]), "UDP 不能转发");
        assert_eq!(
            forward_preset(&list[3]),
            ForwardPreset {
                target_host: "127.0.0.1".into(),
                port: 80,
                name: "nginx".into(),
            }
        );
        assert_eq!(
            forward_preset(&list[0]).target_host,
            "127.0.0.1",
            "0.0.0.0 经 127.0.0.1 访问"
        );
        let specific = conn("tcp", "LISTEN", "10.0.0.5", 8080);
        let preset = forward_preset(&specific);
        assert_eq!(preset.target_host, "10.0.0.5");
        assert_eq!(preset.name, "");
    }

    #[test]
    fn connection_keys_are_unique() {
        let mut list = conns();
        list.push(list[2].clone());
        let keys = conn_keys(&list);
        let unique: HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
        assert_eq!(keys[4], format!("{}#2", keys[2]));
    }
}
