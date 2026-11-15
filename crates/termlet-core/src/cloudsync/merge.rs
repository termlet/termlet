//! 合并：每条记录与本机上次同步成功时的基准（版本与内容摘要）比较，
//! 得出哪一边修改、新增或删除了它：
//!
//! - 只有一边改了：采用改了的那边（另一边写入或删除）；
//! - 两边都改了且内容相同：只更新基准；
//! - 两边都改了且不同（冲突）：修改时间较新的一边胜出；一边删除、一边修改时保留修改（不丢数据）。冲突列给用户看。
//!
//! 删除不需要对方的删除记录：基准中有、某一边没有了，就是那一边删除了；基准只在同步成功后更新。之后保持引用完整
//! （见 [`keep_referenced`]）：同步后仍在的记录引用的记录不删除；删改冲突中保留的条目，它的秘密、主机下属的端口转发
//! 一并保留。本机上次写到云端的内容不在云端时（被其他设备的旧文件覆盖、云端回退），不采纳云端的删除，本机后来写过的
//! 记录不采纳云端的旧版本，本机后来删除的记录重新删除（[`PlanOpts::lost`]）。没能写入本机的记录（本机不认识或不合法，
//! 例如来自其他版本）的基准另做标记，本机的值不当作修改推上云端，升级后重新尝试写入（见 [`Base::unapplied`]）。

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::records::{
    Data, Local, Locals, Remote, Remotes, SECRET, applicable, companions, countable, item_kind,
    kind_of, name_of, owner_of, refs_of, secret_key, version, version_time,
};
use super::{RecordKind, Side};
use crate::model::Kind;

/// 本机上次同步时一条记录的版本与内容摘要。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Base {
    pub v: String,
    pub h: String,
}

pub(crate) type Bases = BTreeMap<String, Base>;

/// 没有写入本机的记录的基准的摘要前缀，见 [`Base::unapplied`]。
const UNAPPLIED: &str = "unapplied:";

impl Base {
    pub fn new(v: &str, h: &str) -> Self {
        Self {
            v: v.to_string(),
            h: h.to_string(),
        }
    }

    /// 没有写入本机的记录的基准：版本是云端的，摘要记下本机当时的内容摘要（本机没有时为空）与应用的版本。
    /// 合并时本机的值不当作本机的修改（之后被用户改了的除外），应用的版本变了（升级）时重新尝试写入云端的值。
    pub fn unapplied(v: &str, local_hash: &str, app: &str) -> Self {
        Self {
            v: v.to_string(),
            h: format!("{UNAPPLIED}{local_hash}:{app}"),
        }
    }

    /// 没有写入本机的记录的基准中记下的本机内容摘要与应用的版本；不是这种基准时为 `None`。
    pub fn unapplied_parts(&self) -> Option<(&str, &str)> {
        let rest = self.h.strip_prefix(UNAPPLIED)?;
        Some(rest.split_once(':').unwrap_or((rest, "")))
    }
}

/// 同步方式。首次同步、本机与云端都已有数据时由用户选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// 按基准合并
    Normal,
    /// 首次同步：合并两边（相同的记录内容不同时按修改时间）
    Merge,
    /// 以云端为准：本机改为与云端相同
    Remote,
    /// 以本机为准：云端改为与本机相同
    Local,
}

/// 合并的方式。
#[derive(Clone, Debug)]
pub(crate) struct PlanOpts {
    pub mode: Mode,
    pub device: String,
    /// 同步时的时间（Unix 毫秒），本机修改时间不明时使用
    pub now: i64,
    /// 本机应用的版本（见 [`Base::unapplied`]）
    pub app: String,
    /// 本机上次写到云端的内容不在云端：云端可能是较早的状态，不采纳云端的删除
    pub lost: bool,
    /// 云端包含的本机最后一次写入之后，本机写到云端的记录：云端的是更早的版本，以本机的为准
    pub wrote_after: HashSet<String>,
    /// 这之后本机从云端删除的记录（键 → 删除的版本）：云端仍是这个版本时重新删除
    pub deleted_after: HashMap<String, String>,
}

impl PlanOpts {
    pub fn new(mode: Mode, device: &str, now: i64, app: &str) -> Self {
        Self {
            mode,
            device: device.to_string(),
            now,
            app: app.to_string(),
            lost: false,
            wrote_after: HashSet::new(),
            deleted_after: HashMap::new(),
        }
    }
}

/// 两边都修改了的一条记录（合并时记下胜出与落败的两边，落败的一边可以由用户改用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictEntry {
    pub key: String,
    pub kind: RecordKind,
    pub name: String,
    pub winner: Side,
    /// 其中一边删除了它（保留了修改的一边）
    pub deleted: bool,
    /// 落败一边的内容；`None` 表示落败的一边删除了它
    pub loser: Option<Data>,
    /// 胜出一边内容的哈希（`None` 为删除）：改用另一边前核对本机现在仍是它，之后又改过时不再覆盖
    pub winner_hash: Option<String>,
}

/// 一次同步要做的事。
#[derive(Clone, Debug, Default)]
pub(crate) struct Plan {
    /// 要写入本机的记录（`None` 表示删除）
    pub to_local: Vec<(String, Option<Remote>)>,
    /// 要写到云端的记录（`None` 表示删除）
    pub to_remote: BTreeMap<String, Option<Remote>>,
    pub conflicts: Vec<ConflictEntry>,
    /// 要在本机、云端删除的条目数（不含设置与秘密）
    pub local_deletes: usize,
    pub remote_deletes: usize,
}

/// 一条记录采用哪一边的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    /// 采用本机的：写到云端或删除云端的
    Local,
    /// 采用云端的：写入本机或删除本机的
    Remote,
}

/// 比较本机、云端与基准。
pub(crate) fn make_plan(locals: &Locals, remote: &Remotes, bases: &Bases, o: &PlanOpts) -> Plan {
    let mut plan = Plan::default();
    let mut keys: Vec<&String> = locals
        .keys()
        .chain(remote.keys())
        .chain(bases.keys())
        .collect();
    keys.sort();
    keys.dedup();

    let mut uses: HashMap<String, Use> = HashMap::new();
    // 一边删除、一边修改而保留了修改的记录
    let mut kept: Vec<String> = Vec::new();
    for key in keys {
        let l = locals.get(key);
        let r = remote.get(key);
        let mut b = bases.get(key);
        if l.is_none() && !applicable(key) {
            // 本机不认识的记录（来自其他版本）：留在云端，不写入本机，也不当作本机删除了它
            continue;
        }
        let same = matches!((l, r), (Some(l), Some(r)) if l.hash == r.hash);
        if l.is_none()
            && r.is_some()
            && b.is_none()
            && kind_of(key) == SECRET
            && owner_of(key).is_some_and(|owner| locals.contains_key(owner))
        {
            // 本机的条目没有秘密、云端有，而本机从没同步过它（重置保险库后重新设置同步）：任何方式下都采用云端的，
            // 不把"没有秘密"推上云端
            uses.insert(key.clone(), Use::Remote);
            continue;
        }
        match o.mode {
            Mode::Remote | Mode::Local => {
                if !same && (l.is_some() || r.is_some()) {
                    let side = if o.mode == Mode::Remote {
                        Use::Remote
                    } else {
                        Use::Local
                    };
                    uses.insert(key.clone(), side);
                }
                continue;
            }
            // 首次合并：只有一边有的保留，两边都有且不同的按修改时间
            Mode::Merge => b = None,
            Mode::Normal => {}
        }
        if o.lost
            && l.is_none()
            && let Some(r) = r
            && o.deleted_after.get(key) == Some(&r.v)
        {
            // 本机写入丢失：云端的正是本机删除了的那个版本，重新删除（其他设备之后修改过时版本不同，保留修改）
            uses.insert(key.clone(), Use::Local);
            continue;
        }
        if o.lost
            && b.is_some()
            && !same
            && (o.wrote_after.contains(key) || (l.is_some() && r.is_none()))
        {
            // 本机写入丢失：本机后来写过的记录，云端的是更早的版本，以本机的为准（本机之后删除了的照常删除）；
            // 云端没有了的记录可能是云端回退、也可能是其他设备基于旧文件写入时删除的，不采纳删除
            if l.is_some() || r.is_some() {
                uses.insert(key.clone(), Use::Local);
            }
            continue;
        }

        let mut local_changed = match (l, b) {
            (Some(l), Some(b)) => l.hash != b.h,
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        let mut remote_changed = match (r, b) {
            (Some(r), Some(b)) => r.v != b.v,
            (Some(_), None) => true,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        if let Some((lh, app)) = b.and_then(Base::unapplied_parts) {
            // 上次没能写入本机的记录：本机的值不是本机的修改（之后被用户改了的除外）；应用的版本变了（升级）、
            // 或当时本机没有而现在有了（升级后认识了这组设置）时，重新尝试写入云端的值
            local_changed = !same && !lh.is_empty() && l.is_none_or(|l| l.hash != lh);
            remote_changed =
                !same && (remote_changed || app != o.app || (lh.is_empty() && l.is_some()));
        }
        match (local_changed, remote_changed) {
            (false, false) => {}
            (true, false) => {
                uses.insert(key.clone(), Use::Local);
            }
            (false, true) => {
                uses.insert(key.clone(), Use::Remote);
            }
            (true, true) => match (l, r) {
                // 两边都删除了；或两边改成了同样的内容
                (None, None) => {}
                _ if same => {}
                (None, Some(r)) => {
                    // 本机删除、云端修改：保留云端的修改
                    uses.insert(key.clone(), Use::Remote);
                    plan.conflict(key, Some(&r.data), Side::Remote, true, None);
                    kept.push(key.clone());
                }
                (Some(l), None) => {
                    // 云端删除、本机修改：保留本机的修改
                    uses.insert(key.clone(), Use::Local);
                    plan.conflict(key, Some(&l.data), Side::Local, true, None);
                    kept.push(key.clone());
                }
                (Some(l), Some(r)) => {
                    // 本机从没改过（默认值）的比任何修改都旧
                    if l.changed > version_time(&r.v) {
                        uses.insert(key.clone(), Use::Local);
                        plan.conflict(key, Some(&l.data), Side::Local, false, Some(&r.data));
                    } else {
                        uses.insert(key.clone(), Use::Remote);
                        plan.conflict(key, Some(&r.data), Side::Remote, false, Some(&l.data));
                    }
                }
            },
        }
    }
    keep_referenced(locals, remote, &mut uses, &kept);

    let mut decided: Vec<(&String, &Use)> = uses.iter().collect();
    decided.sort_by(|a, b| a.0.cmp(b.0));
    for (key, side) in decided {
        match side {
            Use::Local => match locals.get(key) {
                None => {
                    plan.to_remote.insert(key.clone(), None);
                    if countable(key) {
                        plan.remote_deletes += 1;
                    }
                }
                Some(l) => {
                    // 内容与基准相同时沿用基准的版本（重新上传、保留被引用的记录）：其他设备不会把它当作新的修改
                    let at = if l.changed > 0 { l.changed } else { o.now };
                    let v = match bases.get(key) {
                        Some(b) if b.h == l.hash => b.v.clone(),
                        _ => version(at, &o.device, &l.hash),
                    };
                    plan.to_remote.insert(
                        key.clone(),
                        Some(Remote {
                            v,
                            data: l.data.clone(),
                            hash: l.hash.clone(),
                        }),
                    );
                }
            },
            Use::Remote => match remote.get(key) {
                None => {
                    plan.to_local.push((key.clone(), None));
                    if countable(key) {
                        plan.local_deletes += 1;
                    }
                }
                Some(r) => plan.to_local.push((key.clone(), Some(r.clone()))),
            },
        }
    }
    plan
}

impl Plan {
    fn conflict(
        &mut self,
        key: &str,
        winner_data: Option<&Data>,
        winner: Side,
        deleted: bool,
        loser: Option<&Data>,
    ) {
        // 秘密随它所属的条目一起显示，不单独列出
        let Some(kind) = RecordKind::of_key(key) else {
            return;
        };
        self.conflicts.push(ConflictEntry {
            key: key.to_string(),
            kind,
            name: name_of(key, winner_data),
            winner,
            deleted,
            loser: loser.cloned(),
            winner_hash: winner_data.map(Data::hash),
        });
    }
}

/// 保持引用完整：同步后仍在的记录引用的记录（上级分组、凭据档案、密钥、跳板机、秘密的归属）不删除，改为采用
/// 还有它的一边，例如一边删除分组、另一边把主机移入这个分组时保留分组；因此保留、写回本机的条目连同它的秘密
/// （主机还有识别出的系统，见 [`companions`]）一并保留；删改冲突中保留了的主机，下属的端口转发一并保留
/// （删除主机时它们随之删除，不是用户单独删除的）。两边都没有的引用在写入时修复。
fn keep_referenced(
    locals: &Locals,
    remote: &Remotes,
    uses: &mut HashMap<String, Use>,
    kept: &[String],
) {
    fn final_data<'a>(
        locals: &'a Locals,
        remote: &'a Remotes,
        uses: &HashMap<String, Use>,
        key: &str,
    ) -> Option<&'a Data> {
        if uses.get(key) == Some(&Use::Remote) {
            remote.get(key).map(|r| &r.data)
        } else {
            locals.get(key).map(|l: &Local| &l.data)
        }
    }
    let revive = |uses: &mut HashMap<String, Use>, key: &str| -> bool {
        if final_data(locals, remote, uses, key).is_some() {
            return false;
        }
        if locals.contains_key(key) {
            uses.insert(key.to_string(), Use::Local);
            true
        } else if remote.contains_key(key) {
            uses.insert(key.to_string(), Use::Remote);
            true
        } else {
            false
        }
    };

    // 主机下属的端口转发
    let forward_prefix = format!("{}/", Kind::Forward.name());
    let mut forwards_of: HashMap<String, Vec<String>> = HashMap::new();
    for (key, data) in locals
        .iter()
        .map(|(k, l)| (k, &l.data))
        .chain(remote.iter().map(|(k, r)| (k, &r.data)))
    {
        if key.starts_with(&forward_prefix) {
            for host in refs_of(key, Some(data)) {
                forwards_of.entry(host).or_default().push(key.clone());
            }
        }
    }

    let mut queue: Vec<String> = locals.keys().chain(remote.keys()).cloned().collect();
    for key in kept {
        let mut deps = companions(key);
        deps.extend(forwards_of.get(key).cloned().unwrap_or_default());
        for dep in deps {
            if revive(uses, &dep) {
                queue.push(dep);
            }
        }
    }
    while let Some(key) = queue.pop() {
        // 从云端写回本机的条目（本机没有：新增的，或本机删除了而另一边修改过）连同它的秘密（主机还有识别出的系统）：
        // 本机可能已经删除了这些秘密（删除条目时一并删除），不能只恢复条目
        if uses.get(&key) == Some(&Use::Remote)
            && remote.contains_key(&key)
            && !locals.contains_key(&key)
            && item_kind(kind_of(&key)).is_some()
        {
            for companion in companions(&key) {
                if revive(uses, &companion) {
                    queue.push(companion);
                }
            }
        }
        let refs = refs_of(&key, final_data(locals, remote, uses, &key));
        for target in refs {
            for dep in [secret_key(&target), target] {
                if revive(uses, &dep) {
                    queue.push(dep);
                }
            }
        }
    }
}

/// 写入本机之后的基准（这次同步中重新比较，以及写到云端时其他设备先写入、下一轮合并都以它为准）：
/// 写入了的记录、写入后本机（`after`）与云端内容相同的记录以云端的为基准（之后本机与它不同就是本机的修改，例如写入时
/// 修复的引用）。没有写入的记录记为没有写入本机（见 [`Base::unapplied`]），记下本机读取时（`before`）的内容：
/// 两边都没有再修改时不再重试，也不会把本机的旧值当作修改上传；用户之后在本机修改了它则照常上传。
pub(crate) fn applied_bases(
    bases: &Bases,
    remote: &Remotes,
    changes: &[(String, Option<Remote>)],
    failed: &HashSet<String>,
    before: &Locals,
    after: &Locals,
    app: &str,
) -> Bases {
    let mut out = bases.clone();
    for (key, _) in changes {
        match remote.get(key) {
            Some(r) if failed.contains(key) => {
                let lh = before.get(key).map(|l| l.hash.as_str()).unwrap_or("");
                out.insert(key.clone(), Base::unapplied(&r.v, lh, app));
            }
            Some(r) => {
                out.insert(key.clone(), Base::new(&r.v, &r.hash));
            }
            None => {
                out.remove(key);
            }
        }
    }
    for (key, r) in remote {
        if after.get(key).is_some_and(|l| l.hash == r.hash) {
            out.insert(key.clone(), Base::new(&r.v, &r.hash));
        }
    }
    out
}

/// 同步成功后的基准：云端（写入后）的各条记录。没有写入本机的记录保持标记（见 [`Base::unapplied`]）：本机不认识的，
/// 以及没有写入、这次也没有上传而本机与云端仍不同的（之前没有写入、两边都没有再修改）。
pub(crate) fn next_bases(
    remote: &Remotes,
    locals: &Locals,
    bases: &Bases,
    pushed: &BTreeMap<String, Option<Remote>>,
    app: &str,
) -> Bases {
    let mut out = Bases::new();
    for (key, r) in remote {
        let mut base = Base::new(&r.v, &r.hash);
        let l = locals.get(key);
        if l.is_none() && !applicable(key) {
            base = Base::unapplied(&r.v, "", app);
        } else if let Some(old) = bases.get(key)
            && old.unapplied_parts().is_some()
            && !pushed.contains_key(key)
            && l.is_none_or(|l| l.hash != r.hash)
        {
            base = old.clone();
        }
        out.insert(key.clone(), base);
    }
    out
}

/// 要删除的数据是否较多：至少 5 条且占三成以上。
pub(crate) fn too_many(deletes: usize, total: usize) -> bool {
    deletes >= 5 && deletes * 10 >= total * 3
}
