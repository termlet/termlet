//! 测试用的内存 SFTP 服务端：与真实服务器一样
//! 并发处理请求。可以让某个偏移的写入或读取失败、拒绝截断文件、拒绝读取某个文件或列出某个目录、让下一次打开的文件
//! 读取或写入时停住、让删除停住、让改名失败，断开全部连接。与 Linux 一样拒绝超过 255 字节的文件名。
//! 同一个文件系统可以开多个连接。符号链接按 OpenSSH 的参数顺序（先目标、后链接）。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Packet, StatusCode, Version,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{mpsc, watch};

use crate::sftp::path;

/// 放行一个停住的操作。
#[derive(Clone)]
pub(crate) struct Gate(Arc<watch::Sender<bool>>);

impl Default for Gate {
    fn default() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }
}

impl Gate {
    fn new() -> Self {
        Self::default()
    }

    pub(crate) fn release(&self) {
        self.0.send_replace(true);
    }

    /// 等到放行。
    pub(crate) async fn wait_open(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|open| *open).await;
    }
}

struct FileNode {
    data: Vec<u8>,
    mode: u32,
    mtime: u32,
}

enum Node {
    Dir { mode: u32, mtime: u32 },
    File(Arc<Mutex<FileNode>>),
    Link(String),
}

enum OpenHandle {
    File {
        node: Arc<Mutex<FileNode>>,
        hold: Option<Gate>,
    },
    Dir {
        entries: Option<Vec<File>>,
    },
}

/// 改名的钩子：返回错误码时这次改名失败。
pub(crate) type RenameHook = Box<dyn Fn(&str, &str) -> Option<StatusCode> + Send + Sync>;

#[derive(Default)]
struct Faults {
    /// 写到这个偏移的请求等更靠后的块写入成功后失败（只失败一次）
    fail_write: Option<(u64, bool, Gate)>,
    /// 读到这个偏移及之后的数据的请求都失败
    fail_read: Option<u64>,
    /// 单次读取最多返回这么多字节（服务器自己限制、又没有声明上限）
    read_cap: Option<u32>,
    /// 收到的读取请求数
    reads: u64,
    /// 这些文件可以查看属性，打开读取时没有权限；目录可以查看属性，列出内容时没有权限
    denied: HashSet<String>,
    hold_read: Option<Gate>,
    hold_write: Option<Gate>,
    no_truncate: bool,
    hold_remove: Option<Gate>,
    posix_rename_err: Option<StatusCode>,
    rename_err: Option<StatusCode>,
    rename_hook: Option<RenameHook>,
}

pub(crate) struct MemFs {
    nodes: Mutex<BTreeMap<String, Node>>,
    handles: Mutex<HashMap<String, OpenHandle>>,
    next_handle: AtomicU64,
    faults: Mutex<Faults>,
    /// 关闭它断开全部连接
    kill: Mutex<Vec<mpsc::Sender<()>>>,
}

fn now() -> u32 {
    (crate::time::now_ms() / 1000) as u32
}

fn status(id: u32, code: StatusCode, message: &str) -> Packet {
    Packet::status(id, code, message, "en-US")
}

fn ok(id: u32) -> Packet {
    status(id, StatusCode::Ok, "Ok")
}

fn fail(id: u32, message: &str) -> Packet {
    status(id, StatusCode::Failure, message)
}

/// 与 Linux 一样拒绝超过 255 字节的文件名。
fn name_too_long(p: &str) -> bool {
    path::base_name(p).len() > 255
}

impl MemFs {
    pub(crate) fn new() -> Arc<Self> {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "/".to_string(),
            Node::Dir {
                mode: 0o755,
                mtime: now(),
            },
        );
        Arc::new(Self {
            nodes: Mutex::new(nodes),
            handles: Mutex::default(),
            next_handle: AtomicU64::new(1),
            faults: Mutex::default(),
            kill: Mutex::default(),
        })
    }

    /// 新开一个连接：返回客户端一侧的流，服务端在核心的运行时上处理。
    pub(crate) fn connect(self: &Arc<Self>) -> DuplexStream {
        let (client, server) = tokio::io::duplex(4 << 20);
        let (kill_tx, kill_rx) = mpsc::channel(1);
        self.kill.lock().unwrap().push(kill_tx);
        crate::runtime::spawn(serve(self.clone(), server, kill_rx));
        client
    }

    /// 断开全部连接（模拟连接断开）。
    pub(crate) fn drop_connections(&self) {
        self.kill.lock().unwrap().clear();
    }

    pub(crate) fn fail_write(&self, offset: u64) {
        self.faults.lock().unwrap().fail_write = Some((offset, false, Gate::new()));
    }

    pub(crate) fn fail_read(&self, offset: Option<u64>) {
        self.faults.lock().unwrap().fail_read = offset;
    }

    /// 单次读取最多返回 `cap` 字节。
    pub(crate) fn cap_reads(&self, cap: Option<u32>) {
        self.faults.lock().unwrap().read_cap = cap;
    }

    /// 到现在为止收到的读取请求数。
    pub(crate) fn read_requests(&self) -> u64 {
        self.faults.lock().unwrap().reads
    }

    pub(crate) fn deny_read(&self, p: &str, deny: bool) {
        let mut faults = self.faults.lock().unwrap();
        if deny {
            faults.denied.insert(p.to_string());
        } else {
            faults.denied.remove(p);
        }
    }

    /// 下一次打开读取的文件在读之前停住，放行后继续。
    pub(crate) fn hold_next_read(&self) -> Gate {
        let gate = Gate::new();
        self.faults.lock().unwrap().hold_read = Some(gate.clone());
        gate
    }

    /// 下一次打开写入的文件在写之前停住，放行后继续。
    pub(crate) fn hold_next_write(&self) -> Gate {
        let gate = Gate::new();
        self.faults.lock().unwrap().hold_write = Some(gate.clone());
        gate
    }

    /// 之后删除文件的请求停住，放行后继续。
    pub(crate) fn hold_removes(&self) -> Gate {
        let gate = Gate::new();
        self.faults.lock().unwrap().hold_remove = Some(gate.clone());
        gate
    }

    pub(crate) fn set_no_truncate(&self, value: bool) {
        self.faults.lock().unwrap().no_truncate = value;
    }

    /// 让 posix-rename、普通的改名返回这些错误（为空时恢复正常）。
    pub(crate) fn fail_rename(&self, posix: Option<StatusCode>, plain: Option<StatusCode>) {
        let mut faults = self.faults.lock().unwrap();
        faults.posix_rename_err = posix;
        faults.rename_err = plain;
    }

    pub(crate) fn on_rename(&self, hook: Option<RenameHook>) {
        self.faults.lock().unwrap().rename_hook = hook;
    }

    // ---- 测试直接查看、修改 ----

    pub(crate) fn put(&self, p: &str, data: &[u8]) {
        let node = Node::File(Arc::new(Mutex::new(FileNode {
            data: data.to_vec(),
            mode: 0o644,
            mtime: now(),
        })));
        self.nodes.lock().unwrap().insert(p.to_string(), node);
    }

    pub(crate) fn mkdir(&self, p: &str) {
        self.nodes.lock().unwrap().insert(
            p.to_string(),
            Node::Dir {
                mode: 0o755,
                mtime: now(),
            },
        );
    }

    pub(crate) fn symlink(&self, target: &str, link: &str) {
        self.nodes
            .lock()
            .unwrap()
            .insert(link.to_string(), Node::Link(target.to_string()));
    }

    /// 文件的内容（不是文件时为空）。
    pub(crate) fn read(&self, p: &str) -> Option<Vec<u8>> {
        match self.nodes.lock().unwrap().get(p)? {
            Node::File(f) => Some(f.lock().unwrap().data.clone()),
            _ => None,
        }
    }

    pub(crate) fn exists(&self, p: &str) -> bool {
        self.nodes.lock().unwrap().contains_key(p)
    }

    /// 目录中的名称（按名称排序）。
    pub(crate) fn names(&self, dir: &str) -> Vec<String> {
        let nodes = self.nodes.lock().unwrap();
        nodes
            .keys()
            .filter(|k| *k != "/" && path::parent(k) == dir)
            .map(|k| path::base_name(k))
            .collect()
    }

    pub(crate) fn size(&self, p: &str) -> Option<u64> {
        self.read(p).map(|d| d.len() as u64)
    }

    pub(crate) fn mode(&self, p: &str) -> Option<u32> {
        match self.nodes.lock().unwrap().get(p)? {
            Node::File(f) => Some(f.lock().unwrap().mode),
            Node::Dir { mode, .. } => Some(*mode),
            Node::Link(_) => Some(0o777),
        }
    }

    // ---- 服务端 ----

    /// 解析路径中的符号链接（`follow` 时连最后一级也解析），返回真实的路径（最后一级可以不存在）。
    fn resolve(
        &self,
        nodes: &BTreeMap<String, Node>,
        p: &str,
        follow: bool,
    ) -> Result<String, StatusCode> {
        let mut queue: VecDeque<String> = path::clean(&path::join("/", p))
            .split('/')
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect();
        let mut current = "/".to_string();
        let mut hops = 0;
        while let Some(part) = queue.pop_front() {
            let next = path::join(&current, &part);
            let last = queue.is_empty();
            match nodes.get(&next) {
                Some(Node::Link(target)) if follow || !last => {
                    hops += 1;
                    if hops > 40 {
                        return Err(StatusCode::Failure);
                    }
                    let target = path::join(&current, target);
                    let mut rest: VecDeque<String> = target
                        .split('/')
                        .filter(|c| !c.is_empty())
                        .map(str::to_string)
                        .collect();
                    rest.extend(queue);
                    queue = rest;
                    current = "/".to_string();
                }
                Some(Node::File(_)) if !last => return Err(StatusCode::NoSuchFile),
                None if !last => return Err(StatusCode::NoSuchFile),
                _ => current = next,
            }
        }
        Ok(current)
    }

    fn attrs_of(node: &Node, target_len: usize) -> FileAttributes {
        let (kind, mode, size, mtime) = match node {
            Node::Dir { mode, mtime } => (0o040000, *mode, 4096, *mtime),
            Node::File(f) => {
                let f = f.lock().unwrap();
                (0o100000, f.mode, f.data.len() as u64, f.mtime)
            }
            Node::Link(_) => (0o120000, 0o777, target_len as u64, now()),
        };
        FileAttributes {
            size: Some(size),
            uid: Some(1000),
            gid: Some(1000),
            user: None,
            group: None,
            permissions: Some(kind | mode),
            atime: Some(mtime),
            mtime: Some(mtime),
        }
    }

    fn stat(&self, p: &str, follow: bool) -> Result<FileAttributes, StatusCode> {
        let nodes = self.nodes.lock().unwrap();
        let real = self.resolve(&nodes, p, follow)?;
        let node = nodes.get(&real).ok_or(StatusCode::NoSuchFile)?;
        let len = match node {
            Node::Link(t) => t.len(),
            _ => 0,
        };
        Ok(Self::attrs_of(node, len))
    }

    fn new_handle(&self, handle: OpenHandle) -> String {
        let id = self.next_handle.fetch_add(1, Ordering::SeqCst).to_string();
        self.handles.lock().unwrap().insert(id.clone(), handle);
        id
    }

    fn file_of(&self, handle: &str) -> Option<(Arc<Mutex<FileNode>>, Option<Gate>)> {
        match self.handles.lock().unwrap().get(handle)? {
            OpenHandle::File { node, hold } => Some((node.clone(), hold.clone())),
            OpenHandle::Dir { .. } => None,
        }
    }

    fn apply(node: &mut Node, attrs: &FileAttributes) {
        match node {
            Node::File(f) => {
                let mut f = f.lock().unwrap();
                if let Some(size) = attrs.size {
                    f.data.resize(size as usize, 0);
                }
                if let Some(perm) = attrs.permissions {
                    f.mode = perm & 0o7777;
                }
                if let Some(mtime) = attrs.mtime {
                    f.mtime = mtime;
                }
            }
            Node::Dir { mode, mtime } => {
                if let Some(perm) = attrs.permissions {
                    *mode = perm & 0o7777;
                }
                if let Some(t) = attrs.mtime {
                    *mtime = t;
                }
            }
            Node::Link(_) => {}
        }
    }

    /// 移动 `from`（连同目录中的内容）到 `to`。
    fn move_node(nodes: &mut BTreeMap<String, Node>, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let keys: Vec<String> = nodes
            .keys()
            .filter(|k| *k == from || k.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys {
            let node = nodes.remove(&key).unwrap();
            let moved = format!("{to}{}", &key[from.len()..]);
            nodes.insert(moved, node);
        }
    }

    async fn handle(self: &Arc<Self>, packet: Packet) -> Option<Packet> {
        let id = packet.get_request_id();
        Some(match packet {
            Packet::Init(_) => Packet::Version(Version {
                version: 3,
                extensions: HashMap::from([("posix-rename@openssh.com".into(), "1".into())]),
            }),
            Packet::RealPath(r) => {
                let nodes = self.nodes.lock().unwrap();
                match self.resolve(&nodes, &r.path, true) {
                    Ok(real) => Packet::Name(Name {
                        id,
                        files: vec![File::dummy(real)],
                    }),
                    Err(code) => status(id, code, "realpath"),
                }
            }
            Packet::Stat(r) => match self.stat(&r.path, true) {
                Ok(attrs) => Packet::Attrs(Attrs { id, attrs }),
                Err(code) => status(id, code, "stat"),
            },
            Packet::Lstat(r) => match self.stat(&r.path, false) {
                Ok(attrs) => Packet::Attrs(Attrs { id, attrs }),
                Err(code) => status(id, code, "lstat"),
            },
            Packet::Fstat(r) => match self.file_of(&r.handle) {
                Some((node, _)) => Packet::Attrs(Attrs {
                    id,
                    attrs: Self::attrs_of(&Node::File(node), 0),
                }),
                None => fail(id, "bad handle"),
            },
            Packet::Open(r) => self.open(id, &r.filename, r.pflags, &r.attrs),
            Packet::Close(r) => {
                self.handles.lock().unwrap().remove(&r.handle);
                ok(id)
            }
            Packet::Read(r) => {
                let Some((node, hold)) = self.file_of(&r.handle) else {
                    return Some(fail(id, "bad handle"));
                };
                if let Some(gate) = hold {
                    gate.wait_open().await;
                }
                let (fail_at, cap) = {
                    let mut faults = self.faults.lock().unwrap();
                    faults.reads += 1;
                    (faults.fail_read, faults.read_cap)
                };
                if fail_at.is_some_and(|at| r.offset + u64::from(r.len) > at) {
                    return Some(fail(id, "模拟的读取失败"));
                }
                let f = node.lock().unwrap();
                let start = r.offset as usize;
                if start >= f.data.len() {
                    return Some(status(id, StatusCode::Eof, "EOF"));
                }
                let len = cap.map_or(r.len, |cap| r.len.min(cap));
                let end = (start + len as usize).min(f.data.len());
                Packet::Data(Data {
                    id,
                    data: f.data[start..end].to_vec(),
                })
            }
            Packet::Write(r) => {
                let Some((node, hold)) = self.file_of(&r.handle) else {
                    return Some(fail(id, "bad handle"));
                };
                if let Some(gate) = hold {
                    gate.wait_open().await;
                }
                let target = {
                    let mut faults = self.faults.lock().unwrap();
                    match faults.fail_write.as_mut() {
                        Some((at, failed, gate)) => {
                            let target = *at;
                            if target == r.offset && !*failed {
                                *failed = true;
                                Some((target, gate.clone(), true))
                            } else {
                                Some((target, gate.clone(), false))
                            }
                        }
                        None => None,
                    }
                };
                if let Some((_, gate, true)) = &target {
                    // 等更靠后的块写入成功后再失败，临时文件中间留下空洞
                    let _ = tokio::time::timeout(Duration::from_secs(5), gate.wait_open()).await;
                    return Some(fail(id, "模拟的写入失败"));
                }
                {
                    let mut f = node.lock().unwrap();
                    let start = r.offset as usize;
                    if f.data.len() < start + r.data.len() {
                        f.data.resize(start + r.data.len(), 0);
                    }
                    f.data[start..start + r.data.len()].copy_from_slice(&r.data);
                    f.mtime = now();
                }
                if let Some((at, gate, false)) = &target
                    && r.offset > *at
                {
                    gate.release();
                }
                ok(id)
            }
            Packet::SetStat(r) => {
                if self.faults.lock().unwrap().no_truncate && r.attrs.size.is_some() {
                    return Some(fail(id, "模拟的截断失败"));
                }
                let mut nodes = self.nodes.lock().unwrap();
                match self.resolve(&nodes, &r.path, true) {
                    Ok(real) => match nodes.get_mut(&real) {
                        Some(node) => {
                            Self::apply(node, &r.attrs);
                            ok(id)
                        }
                        None => status(id, StatusCode::NoSuchFile, "setstat"),
                    },
                    Err(code) => status(id, code, "setstat"),
                }
            }
            Packet::FSetStat(r) => match self.file_of(&r.handle) {
                Some((node, _)) => {
                    Self::apply(&mut Node::File(node), &r.attrs);
                    ok(id)
                }
                None => fail(id, "bad handle"),
            },
            Packet::OpenDir(r) => self.open_dir(id, &r.path),
            Packet::ReadDir(r) => {
                let mut handles = self.handles.lock().unwrap();
                match handles.get_mut(&r.handle) {
                    Some(OpenHandle::Dir { entries }) => match entries.take() {
                        Some(files) if !files.is_empty() => Packet::Name(Name { id, files }),
                        _ => status(id, StatusCode::Eof, "EOF"),
                    },
                    _ => fail(id, "bad handle"),
                }
            }
            Packet::Remove(r) => {
                let hold = self.faults.lock().unwrap().hold_remove.clone();
                if let Some(gate) = hold {
                    gate.wait_open().await;
                }
                let mut nodes = self.nodes.lock().unwrap();
                match self.resolve(&nodes, &r.filename, false) {
                    Ok(real) => match nodes.get(&real) {
                        Some(Node::Dir { .. }) => fail(id, "is a directory"),
                        Some(_) => {
                            nodes.remove(&real);
                            ok(id)
                        }
                        None => status(id, StatusCode::NoSuchFile, "remove"),
                    },
                    Err(code) => status(id, code, "remove"),
                }
            }
            Packet::MkDir(r) => {
                if name_too_long(&r.path) {
                    return Some(fail(id, "文件名过长"));
                }
                let mut nodes = self.nodes.lock().unwrap();
                match self.resolve(&nodes, &r.path, false) {
                    Ok(real) if nodes.contains_key(&real) => fail(id, "已存在"),
                    Ok(real) => {
                        if !matches!(nodes.get(&path::parent(&real)), Some(Node::Dir { .. })) {
                            return Some(status(id, StatusCode::NoSuchFile, "mkdir"));
                        }
                        nodes.insert(
                            real,
                            Node::Dir {
                                mode: 0o755,
                                mtime: now(),
                            },
                        );
                        ok(id)
                    }
                    Err(code) => status(id, code, "mkdir"),
                }
            }
            Packet::RmDir(r) => {
                let mut nodes = self.nodes.lock().unwrap();
                match self.resolve(&nodes, &r.path, false) {
                    Ok(real) => {
                        let prefix = format!("{real}/");
                        if !matches!(nodes.get(&real), Some(Node::Dir { .. })) {
                            status(id, StatusCode::NoSuchFile, "rmdir")
                        } else if nodes.keys().any(|k| k.starts_with(&prefix)) {
                            fail(id, "Directory not empty")
                        } else {
                            nodes.remove(&real);
                            ok(id)
                        }
                    }
                    Err(code) => status(id, code, "rmdir"),
                }
            }
            Packet::Rename(r) => {
                let (err, hooked) = {
                    let faults = self.faults.lock().unwrap();
                    (
                        faults.rename_err,
                        faults
                            .rename_hook
                            .as_ref()
                            .and_then(|h| h(&r.oldpath, &r.newpath)),
                    )
                };
                if let Some(code) = err.or(hooked) {
                    return Some(status(id, code, "模拟的改名失败"));
                }
                self.rename(id, &r.oldpath, &r.newpath, false)
            }
            Packet::Extended(r) if r.request == "posix-rename@openssh.com" => {
                if let Some(code) = self.faults.lock().unwrap().posix_rename_err {
                    return Some(status(id, code, "模拟的 posix-rename 失败"));
                }
                let Some((from, to)) = two_strings(&r.data) else {
                    return Some(status(id, StatusCode::BadMessage, "bad message"));
                };
                self.rename(id, &from, &to, true)
            }
            Packet::Extended(_) => status(id, StatusCode::OpUnsupported, "unsupported"),
            Packet::ReadLink(r) => match self.nodes.lock().unwrap().get(&path::clean(&r.path)) {
                Some(Node::Link(target)) => Packet::Name(Name {
                    id,
                    files: vec![File::dummy(target.clone())],
                }),
                _ => status(id, StatusCode::NoSuchFile, "readlink"),
            },
            // OpenSSH 的顺序：第一个是目标，第二个是链接
            Packet::Symlink(r) => {
                let mut nodes = self.nodes.lock().unwrap();
                let link = path::clean(&r.targetpath);
                match nodes.entry(link) {
                    std::collections::btree_map::Entry::Occupied(_) => fail(id, "已存在"),
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(Node::Link(r.linkpath));
                        ok(id)
                    }
                }
            }
            _ => status(id, StatusCode::OpUnsupported, "unsupported"),
        })
    }

    fn open(&self, id: u32, filename: &str, flags: OpenFlags, attrs: &FileAttributes) -> Packet {
        let write = flags.contains(OpenFlags::WRITE);
        if write && name_too_long(filename) {
            return fail(id, "文件名过长");
        }
        let mut nodes = self.nodes.lock().unwrap();
        let real = match self.resolve(&nodes, filename, true) {
            Ok(real) => real,
            Err(code) => return status(id, code, "open"),
        };
        let node = match nodes.get(&real) {
            Some(Node::Dir { .. }) => return fail(id, "is a directory"),
            Some(Node::File(f)) => {
                if flags.contains(OpenFlags::CREATE) && flags.contains(OpenFlags::EXCLUDE) {
                    return fail(id, "已存在");
                }
                if flags.contains(OpenFlags::TRUNCATE) {
                    f.lock().unwrap().data.clear();
                }
                f.clone()
            }
            Some(Node::Link(_)) => return status(id, StatusCode::NoSuchFile, "dangling"),
            None => {
                if !flags.contains(OpenFlags::CREATE) {
                    return status(id, StatusCode::NoSuchFile, "open");
                }
                if !matches!(nodes.get(&path::parent(&real)), Some(Node::Dir { .. })) {
                    return status(id, StatusCode::NoSuchFile, "open");
                }
                let node = Arc::new(Mutex::new(FileNode {
                    data: Vec::new(),
                    mode: attrs.permissions.map(|p| p & 0o7777).unwrap_or(0o644),
                    mtime: now(),
                }));
                nodes.insert(real.clone(), Node::File(node.clone()));
                node
            }
        };
        drop(nodes);
        let mut faults = self.faults.lock().unwrap();
        let read = flags.contains(OpenFlags::READ);
        if read && (faults.denied.contains(&real) || faults.denied.contains(&path::clean(filename)))
        {
            return status(id, StatusCode::PermissionDenied, "permission denied");
        }
        let hold = if read {
            faults.hold_read.take()
        } else if write {
            faults.hold_write.take()
        } else {
            None
        };
        drop(faults);
        let handle = self.new_handle(OpenHandle::File { node, hold });
        Packet::Handle(Handle { id, handle })
    }

    fn open_dir(&self, id: u32, p: &str) -> Packet {
        let nodes = self.nodes.lock().unwrap();
        let real = match self.resolve(&nodes, p, true) {
            Ok(real) => real,
            Err(code) => return status(id, code, "opendir"),
        };
        if !matches!(nodes.get(&real), Some(Node::Dir { .. })) {
            return status(id, StatusCode::NoSuchFile, "opendir");
        }
        {
            let faults = self.faults.lock().unwrap();
            if faults.denied.contains(&real) || faults.denied.contains(&path::clean(p)) {
                return status(id, StatusCode::PermissionDenied, "permission denied");
            }
        }
        let files: Vec<File> = nodes
            .iter()
            .filter(|(k, _)| *k != "/" && path::parent(k) == real)
            .map(|(k, node)| {
                let len = match node {
                    Node::Link(t) => t.len(),
                    _ => 0,
                };
                File::new(path::base_name(k), Self::attrs_of(node, len))
            })
            .collect();
        drop(nodes);
        let handle = self.new_handle(OpenHandle::Dir {
            entries: Some(files),
        });
        Packet::Handle(Handle { id, handle })
    }

    fn rename(&self, id: u32, from: &str, to: &str, replace: bool) -> Packet {
        if name_too_long(to) {
            return fail(id, "文件名过长");
        }
        let mut nodes = self.nodes.lock().unwrap();
        let (from, to) = match (
            self.resolve(&nodes, from, false),
            self.resolve(&nodes, to, false),
        ) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(code), _) | (_, Err(code)) => return status(id, code, "rename"),
        };
        if !nodes.contains_key(&from) {
            return status(id, StatusCode::NoSuchFile, "rename");
        }
        if !matches!(nodes.get(&path::parent(&to)), Some(Node::Dir { .. })) {
            return status(id, StatusCode::NoSuchFile, "rename");
        }
        match nodes.get(&to) {
            Some(_) if !replace => return fail(id, "已存在"),
            Some(Node::Dir { .. }) => return fail(id, "is a directory"),
            Some(_) => {
                nodes.remove(&to);
            }
            None => {}
        }
        Self::move_node(&mut nodes, &from, &to);
        ok(id)
    }
}

/// 扩展请求中的两个 SSH 字符串。
fn two_strings(data: &[u8]) -> Option<(String, String)> {
    let mut rest = data;
    let mut take = || {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let text = String::from_utf8(rest.get(4..4 + len)?.to_vec()).ok()?;
        rest = &rest[4 + len..];
        Some(text)
    };
    let a = take()?;
    let b = take()?;
    Some((a, b))
}

/// 处理一个连接：每个请求一个任务（并发处理），回应经通道写回。
async fn serve(fs: Arc<MemFs>, stream: DuplexStream, mut kill: mpsc::Receiver<()>) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    let write = crate::runtime::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = writer.shutdown().await;
    });
    let mut killed = false;
    loop {
        let len = tokio::select! {
            len = reader.read_u32() => len,
            _ = kill.recv() => {
                killed = true;
                break;
            }
        };
        let Ok(len) = len else {
            break;
        };
        let mut buf = vec![0u8; len as usize];
        if reader.read_exact(&mut buf).await.is_err() {
            break;
        }
        let mut bytes = Bytes::from(buf);
        let Ok(packet) = Packet::try_from(&mut bytes) else {
            break;
        };
        let fs = fs.clone();
        let tx = tx.clone();
        crate::runtime::spawn(async move {
            if let Some(reply) = fs.handle(packet).await
                && let Ok(bytes) = Bytes::try_from(reply)
            {
                let _ = tx.send(bytes);
            }
        });
    }
    drop(tx);
    // 断开时立即关闭；客户端关闭时等还在处理的请求（例如停住的删除）回应后再关闭，与没有响应的连接一样
    if killed {
        write.abort();
    }
}
